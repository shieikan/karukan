import Foundation

/// Newline-delimited JSON-RPC 2.0 client for karukan-imserver.
///
/// Requests are written to the child's stdin; a dedicated reader queue
/// splits stdout on 0x0A and dispatches responses to pending completions.
/// Key processing uses the synchronous API (the IMK `handle` callback must
/// answer "consumed?" synchronously, the same trade-off Mozc makes); slow
/// or fire-and-forget calls use the async API.
class EngineClient {
    private let serverProcess: EngineProcess
    private var nextID = 1
    private let requestQueue = DispatchQueue(label: "dev.togatoga.karukan.jsonrpc.request")

    private let lock = NSLock()
    private struct PendingRequest {
        let sessionEpoch: UInt64
        let connectionEpoch: UInt64
        let completion: (Data?) -> Void
    }

    private var pendingRequests: [Int: PendingRequest] = [:]
    private var sessionEpoch: UInt64 = 0
    private var connectionEpoch: UInt64 = 0

    /// `autoInit` re-sends `init` whenever the server (re)starts. Tests
    /// disable it to avoid loading models.
    init(serverProcess: EngineProcess, autoInit: Bool = true) {
        self.serverProcess = serverProcess
        self.serverProcess.onRestart = { [weak self] in
            self?.beginNewSession()
            self?.startReaderLoop()
            if autoInit {
                self?.initAsync()
            }
        }
    }

    // MARK: - Engine methods

    func initAsync() {
        sendRequest(method: "init", params: [:]) { [weak self] data in
            guard let self else { return }
            guard let data,
                let result = try? makeProtocolDecoder().decode(InitResult.self, from: data)
            else {
                NSLog("KarukanIME: engine init failed")
                return
            }
            self.serverProcess.resetBackoff()
            NSLog(
                "KarukanIME: engine initialized (protocol v\(result.protocolVersion), model=\(result.modelName))"
            )
        }
    }

    func processKeySync(_ key: EngineKeyEvent) -> KeyResult? {
        let params: [String: Any] = [
            "keysym": key.keysym,
            "modifiers": key.modifiers.jsonObject,
            "is_release": false,
        ]
        return keyResultSync(method: "process_key", params: params, timeout: 3.0)
    }

    func pollAsyncConversion(completion: @escaping (KeyResult?) -> Void) {
        sendRequest(method: "poll_async_conversion", params: [:]) { data in
            guard let data,
                let result = try? makeProtocolDecoder().decode(KeyResult.self, from: data)
            else {
                completion(nil)
                return
            }
            completion(result)
        }
    }

    func commitSync() -> KeyResult? {
        keyResultSync(method: "commit", params: [:], timeout: 1.0)
    }

    func saveLearningAsync() {
        sendRequest(method: "save_learning", params: [:]) { _ in }
    }

    func setSurroundingTextAsync(text: String, cursorPos: Int) {
        sendRequest(
            method: "set_surrounding_text",
            params: ["text": text, "cursor_pos": cursorPos]
        ) { _ in }
    }

    /// Reset the server state behind a monotonically increasing session epoch.
    /// The reset request is queued after the epoch barrier, so requests queued
    /// by the previous session cannot write into the new session. Callers can
    /// enqueue new context/key requests from the reset completion and retain
    /// FIFO ordering on the same request queue.
    func resetSessionAsync(completion: (() -> Void)? = nil) {
        advanceSessionEpoch()
        sendRequest(method: "reset", params: [:]) { _ in
            completion?()
        }
    }

    private func keyResultSync(method: String, params: [String: Any], timeout: TimeInterval)
        -> KeyResult?
    {
        guard let data = sendRequestSync(method: method, params: params, timeout: timeout) else {
            return nil
        }
        do {
            return try makeProtocolDecoder().decode(KeyResult.self, from: data)
        } catch {
            NSLog("KarukanIME: failed to decode \(method) result: \(error)")
            return nil
        }
    }

    // MARK: - JSON-RPC transport

    func startReaderLoop() {
        guard let stdout = serverProcess.stdoutPipe else { return }

        let queue = DispatchQueue(label: "dev.togatoga.karukan.jsonrpc.reader")
        let readerEpoch = currentConnectionEpoch()
        queue.async { [weak self] in
            let handle = stdout.fileHandleForReading
            var buffer = Data()

            while true {
                let chunk = handle.availableData
                if chunk.isEmpty {
                    // EOF: server terminated
                    self?.failAllPending(connectionEpoch: readerEpoch)
                    break
                }
                buffer.append(chunk)

                while let newlineRange = buffer.range(of: Data([0x0A])) {
                    let lineData = buffer.subdata(in: buffer.startIndex..<newlineRange.lowerBound)
                    buffer.removeSubrange(buffer.startIndex...newlineRange.lowerBound)
                    guard !lineData.isEmpty else { continue }
                    self?.handleResponse(lineData, connectionEpoch: readerEpoch)
                }
            }
        }
    }

    @discardableResult
    func sendRequest(
        method: String, params: [String: Any], completion: @escaping (Data?) -> Void
    ) -> Int {
        lock.lock()
        let id = nextID
        nextID += 1
        let requestEpoch = sessionEpoch
        let requestConnectionEpoch = connectionEpoch
        pendingRequests[id] = PendingRequest(
            sessionEpoch: requestEpoch,
            connectionEpoch: requestConnectionEpoch,
            completion: completion
        )
        lock.unlock()

        let request: [String: Any] = [
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        ]

        requestQueue.async { [weak self] in
            guard let self,
                self.isCurrentConnection(requestConnectionEpoch),
                let stdin = self.serverProcess.stdinPipe,
                var data = try? JSONSerialization.data(withJSONObject: request)
            else {
                self?.takePending(
                    id: id,
                    sessionEpoch: requestEpoch,
                    connectionEpoch: requestConnectionEpoch
                )?.completion(nil)
                return
            }
            data.append(0x0A)
            do {
                try stdin.fileHandleForWriting.write(contentsOf: data)
            } catch {
                NSLog("KarukanIME: failed to write request: \(error)")
                self.takePending(
                    id: id,
                    sessionEpoch: requestEpoch,
                    connectionEpoch: requestConnectionEpoch
                )?.completion(nil)
            }
        }
        return id
    }

    func sendRequestSync(method: String, params: [String: Any], timeout: TimeInterval) -> Data? {
        let semaphore = DispatchSemaphore(value: 0)
        var result: Data?
        let id = sendRequest(method: method, params: params) { data in
            result = data
            semaphore.signal()
        }
        if semaphore.wait(timeout: .now() + timeout) == .timedOut {
            NSLog("KarukanIME: \(method) timed out after \(timeout)s")
            takePending(
                id: id,
                sessionEpoch: currentSessionEpoch(),
                connectionEpoch: currentConnectionEpoch()
            )?.completion(nil)
            return nil
        }
        return result
    }

    private func handleResponse(_ lineData: Data, connectionEpoch: UInt64) {
        guard
            let json = try? JSONSerialization.jsonObject(with: lineData) as? [String: Any]
        else {
            NSLog("KarukanIME: unparsable response line")
            return
        }
        guard let id = json["id"] as? Int else {
            // id:null happens only for parse errors on our side; log and drop.
            NSLog("KarukanIME: response without id: \(json)")
            return
        }
        if let error = json["error"] as? [String: Any] {
            NSLog("KarukanIME: engine error for request \(id): \(error)")
            takePending(
                id: id,
                sessionEpoch: currentSessionEpoch(),
                connectionEpoch: connectionEpoch
            )?.completion(nil)
            return
        }
        guard let result = json["result"],
            let data = try? JSONSerialization.data(withJSONObject: result)
        else {
            takePending(
                id: id,
                sessionEpoch: currentSessionEpoch(),
                connectionEpoch: connectionEpoch
            )?.completion(nil)
            return
        }
        takePending(
            id: id,
            sessionEpoch: currentSessionEpoch(),
            connectionEpoch: connectionEpoch
        )?.completion(data)
    }

    private func takePending(
        id: Int, sessionEpoch: UInt64, connectionEpoch: UInt64
    ) -> PendingRequest? {
        lock.lock()
        defer { lock.unlock() }
        guard let request = pendingRequests[id],
            request.sessionEpoch == sessionEpoch,
            request.connectionEpoch == connectionEpoch
        else {
            return nil
        }
        return pendingRequests.removeValue(forKey: id)
    }

    private func failAllPending(connectionEpoch: UInt64) {
        lock.lock()
        let pending = pendingRequests.filter { $0.value.connectionEpoch == connectionEpoch }
        for (id, _) in pending {
            pendingRequests.removeValue(forKey: id)
        }
        lock.unlock()
        for (_, request) in pending {
            request.completion(nil)
        }
    }

    private func currentSessionEpoch() -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return sessionEpoch
    }

    private func currentConnectionEpoch() -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return connectionEpoch
    }

    /// Advance the session before enqueuing the reset barrier. Pending old
    /// responses are failed immediately, while already queued writes on the
    /// same connection remain ahead of the reset request in FIFO order.
    @discardableResult
    private func advanceSessionEpoch() -> UInt64 {
        lock.lock()
        sessionEpoch &+= 1
        let newEpoch = sessionEpoch
        let stale = pendingRequests.filter { $0.value.sessionEpoch != newEpoch }
        for (id, _) in stale {
            pendingRequests.removeValue(forKey: id)
        }
        lock.unlock()

        for (_, request) in stale {
            request.completion(nil)
        }

        let barrierConnectionEpoch = currentConnectionEpoch()
        requestQueue.async { [weak self] in
            guard let self, self.isCurrentConnection(barrierConnectionEpoch) else { return }
            // This no-op is an explicit FIFO barrier. Requests queued after
            // advanceSessionEpoch cannot overtake it on requestQueue.
        }
        return newEpoch
    }

    private func beginNewSession() {
        lock.lock()
        connectionEpoch &+= 1
        lock.unlock()
        advanceSessionEpoch()
    }

    private func isCurrentConnection(_ epoch: UInt64) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return connectionEpoch == epoch
    }
}
