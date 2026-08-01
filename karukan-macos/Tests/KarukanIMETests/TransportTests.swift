import InputMethodKit
import XCTest

@testable import KarukanIME

private final class DeferredPollTransport: KarukanEngineTransport {
    var keyResult = KeyResult(
        consumed: true, actions: [.commit(text: "current")], conversionMs: 0, processKeyMs: 0,
        pendingAsync: true)
    var commitResult = KeyResult(
        consumed: true, actions: [], conversionMs: 0, processKeyMs: 0, pendingAsync: false)
    var pollCompletions: [(KeyResult?) -> Void] = []
    var pollCount = 0
    var onPoll: (() -> Void)?

    func processKey(_ key: EngineKeyEvent) -> KeyResult? {
        keyResult
    }

    func pollAsyncConversion(completion: @escaping (KeyResult?) -> Void) {
        pollCount += 1
        pollCompletions.append(completion)
        onPoll?()
    }

    func commitSync() -> KeyResult? { commitResult }
    func resetSessionAsync(completion: (() -> Void)?) { completion?() }
}

private final class RecordingTextClient: NSObject, IMKTextInput {
    private(set) var insertedTexts: [String] = []

    @objc func insertText(_ text: Any, replacementRange: NSRange) {
        insertedTexts.append(String(describing: text))
    }

    func resetInsertedTexts() {
        insertedTexts.removeAll()
    }

    func setMarkedText(_ string: Any!, selectionRange: NSRange, replacementRange: NSRange) {}
    func selectedRange() -> NSRange { NSRange(location: 0, length: 0) }
    func markedRange() -> NSRange { NSRange(location: NSNotFound, length: 0) }
    func attributedSubstring(from range: NSRange) -> NSAttributedString! { nil }
    func length() -> Int { 0 }
    func characterIndex(
        for point: NSPoint, tracking mappingMode: IMKLocationToOffsetMappingMode,
        inMarkedRange: UnsafeMutablePointer<ObjCBool>!
    ) -> Int { 0 }
    func attributes(
        forCharacterIndex index: Int, lineHeightRectangle lineRect: UnsafeMutablePointer<NSRect>!
    ) -> [AnyHashable: Any]! { [:] }
    func validAttributesForMarkedText() -> [Any]! { [] }
    func overrideKeyboard(withKeyboardNamed keyboardUniqueName: String!) {}
    func selectMode(_ modeIdentifier: String!) {}
    func supportsUnicode() -> Bool { true }
    func bundleIdentifier() -> String! { "dev.togatoga.karukan.tests" }
    func windowLevel() -> CGWindowLevel { 0 }
    func supportsProperty(_ property: TSMDocumentPropertyTag) -> Bool { false }
    func uniqueClientIdentifierString() -> String! { "karukan-tests" }
    func string(from range: NSRange, actualRange: NSRangePointer!) -> String! { nil }
    func firstRect(forCharacterRange range: NSRange, actualRange: NSRangePointer!) -> NSRect {
        .zero
    }
}

final class CompletionPollGenerationTests: XCTestCase {
    private enum NewerBoundary {
        case key
        case commit
        case reset
    }

    private func assertDelayedPollIsRejected(after boundary: NewerBoundary) throws {
        let textClient = RecordingTextClient()
        let controller = try XCTUnwrap(
            KarukanInputController(server: nil, delegate: nil, client: nil))
        let client: any IMKTextInput = textClient
        let transport = DeferredPollTransport()
        controller.engineTransport = transport

        let pollStarted = expectation(description: "initial poll started")
        transport.onPoll = { pollStarted.fulfill() }
        _ = controller.processEngineKey(
            EngineKeyEvent(keysym: 0x61, modifiers: KeyModifiers()), client: client)
        wait(for: [pollStarted], timeout: 1.0)
        XCTAssertEqual(textClient.insertedTexts, ["current"])
        textClient.resetInsertedTexts()
        let delayedPoll = try XCTUnwrap(transport.pollCompletions.first)
        transport.onPoll = nil

        switch boundary {
        case .key:
            let newerPollStarted = expectation(description: "newer key poll started")
            transport.onPoll = { newerPollStarted.fulfill() }
            transport.keyResult = KeyResult(
                consumed: true, actions: [], conversionMs: 0, processKeyMs: 0,
                pendingAsync: true)
            _ = controller.processEngineKey(
                EngineKeyEvent(keysym: 0x62, modifiers: KeyModifiers()), client: client)
            wait(for: [newerPollStarted], timeout: 1.0)
            transport.onPoll = nil
        case .commit:
            controller.flushEngineComposition(client: client)
        case .reset:
            controller.resetEngineSession()
        }
        let pollsAfterBoundary = transport.pollCount

        delayedPoll(
            KeyResult(
                consumed: false, actions: [.commit(text: "stale")], conversionMs: 0,
                processKeyMs: 0, pendingAsync: true))
        RunLoop.main.run(until: Date().addingTimeInterval(0.05))

        XCTAssertEqual(textClient.insertedTexts, [])
        XCTAssertEqual(transport.pollCount, pollsAfterBoundary)
    }

    func testNewerKeyRejectsDelayedPollActionsAndReschedule() throws {
        try assertDelayedPollIsRejected(after: .key)
    }

    func testCommitRejectsDelayedPollActionsAndReschedule() throws {
        try assertDelayedPollIsRejected(after: .commit)
    }

    func testResetRejectsDelayedPollActionsAndReschedule() throws {
        try assertDelayedPollIsRejected(after: .reset)
    }
}

/// Integration tests driving a real karukan-imserver binary through
/// EngineProcess + EngineClient. Skipped when the Rust binary hasn't been
/// built (run `cargo build -p karukan-im --bin karukan-imserver` first;
/// `make test` does this automatically).
///
/// Only config-independent requests are exercised: the server loads the
/// user's config.toml, so anything involving conversion behavior is
/// covered by the Rust-side tests instead.
final class TransportTests: XCTestCase {
    static func serverBinaryPath() -> String? {
        // <repo>/karukan-macos/Tests/KarukanIMETests/TransportTests.swift
        let repoRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()  // KarukanIMETests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // karukan-macos
            .deletingLastPathComponent()  // repo root
        for profile in ["release", "debug"] {
            let candidate =
                repoRoot
                .appendingPathComponent("target/\(profile)/karukan-imserver").path
            if FileManager.default.fileExists(atPath: candidate) {
                return candidate
            }
        }
        return nil
    }

    private var process: EngineProcess!
    private var client: EngineClient!

    override func setUpWithError() throws {
        guard let path = Self.serverBinaryPath() else {
            throw XCTSkip("karukan-imserver not built")
        }
        process = EngineProcess(serverPath: path)
        client = EngineClient(serverProcess: process, autoInit: false)
        process.start()
        client.startReaderLoop()
    }

    override func tearDown() {
        process?.stop()
    }

    func testStatusRoundTrip() throws {
        let data = client.sendRequestSync(method: "status", params: [:], timeout: 5.0)
        let json = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: XCTUnwrap(data)) as? [String: Any])
        XCTAssertEqual(json["initialized"] as? Bool, false)
        XCTAssertEqual(json["state"] as? String, "empty")
    }

    func testEscapeInEmptyStateNotConsumed() throws {
        let key = EngineKeyEvent(keysym: 0xff1b, modifiers: KeyModifiers())
        let result = try XCTUnwrap(client.processKeySync(key))
        XCTAssertFalse(result.consumed)
    }

    func testUnknownMethodReturnsNil() {
        let data = client.sendRequestSync(method: "no_such_method", params: [:], timeout: 5.0)
        XCTAssertNil(data)
    }

    func testManySequentialRequests() throws {
        // The reader loop must keep request/response pairing intact.
        for _ in 0..<50 {
            let data = client.sendRequestSync(method: "status", params: [:], timeout: 5.0)
            XCTAssertNotNil(data)
        }
    }

    func testAsyncPollRoundTripDoesNotRequireAnotherKey() throws {
        let data = client.sendRequestSync(
            method: "poll_async_conversion", params: [:], timeout: 1.0)
        let json = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: XCTUnwrap(data)) as? [String: Any])
        XCTAssertEqual(json["consumed"] as? Bool, false)
        XCTAssertNotNil(json["actions"] as? [Any])
        XCTAssertNotNil(json["pending_async"] as? Bool)
    }

    func testSessionResetBarrierIsFIFOAndRealKeyPathStaysBelowThreeSeconds() throws {
        let resetCompleted = expectation(description: "reset barrier completed")
        client.setSurroundingTextAsync(text: "古い文脈", cursorPos: 4)
        client.resetSessionAsync {
            resetCompleted.fulfill()
        }
        wait(for: [resetCompleted], timeout: 1.0)

        client.setSurroundingTextAsync(text: "新しい文脈", cursorPos: 5)
        let start = DispatchTime.now().uptimeNanoseconds
        let key = EngineKeyEvent(keysym: 0x61, modifiers: KeyModifiers())
        let result = try XCTUnwrap(client.processKeySync(key))
        let elapsed = Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000_000

        XCTAssertLessThan(elapsed, 3.0)
        XCTAssertTrue(result.actions.contains { action in
            if case .updatePreedit = action { return true }
            return false
        })
    }

    func testServerStopAndRestartRecovers() throws {
        // restart() waits for the old process off the main thread and
        // completes via onRestart on the main queue; wait(for:) pumps the
        // run loop so that completion can fire.
        let restarted = expectation(description: "server restarted")
        let previousOnRestart = process.onRestart
        process.onRestart = {
            previousOnRestart?()
            restarted.fulfill()
        }
        process.restart()
        wait(for: [restarted], timeout: 5.0)
        let data = client.sendRequestSync(method: "status", params: [:], timeout: 5.0)
        XCTAssertNotNil(data)
    }
}
