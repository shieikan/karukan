import XCTest

@testable import KarukanIME

final class EngineProtocolTests: XCTestCase {
    func decodeKeyResult(_ json: String) throws -> KeyResult {
        try makeProtocolDecoder().decode(KeyResult.self, from: Data(json.utf8))
    }

    func testDecodePreeditAction() throws {
        let json = """
            {"consumed":true,"actions":[{"attributes":[{"end":1,"start":0,"style":"underline"}],"caret":1,"text":"か","type":"update_preedit"}],"conversion_ms":0,"process_key_ms":0}
            """
        let result = try decodeKeyResult(json)
        XCTAssertTrue(result.consumed)
        guard case .updatePreedit(let text, let caret, let attributes) = result.actions[0] else {
            return XCTFail("expected update_preedit")
        }
        XCTAssertEqual(text, "か")
        XCTAssertEqual(caret, 1)
        XCTAssertEqual(attributes.count, 1)
        XCTAssertEqual(attributes[0].style, "underline")
    }

    func testDecodeShowCandidates() throws {
        let json = """
            {"consumed":true,"actions":[{"candidates":[{"description":"[全]カタカナ","text":"カ"},{"text":"か"}],"cursor":0,"page":0,"total_pages":3,"type":"show_candidates"}],"conversion_ms":11,"process_key_ms":42,"pending_async":false}
            """
        let result = try decodeKeyResult(json)
        guard
            case .showCandidates(let candidates, let cursor, let page, let totalPages) =
                result.actions[0]
        else {
            return XCTFail("expected show_candidates")
        }
        XCTAssertEqual(candidates.count, 2)
        XCTAssertEqual(candidates[0].text, "カ")
        XCTAssertEqual(candidates[0].description, "[全]カタカナ")
        XCTAssertNil(candidates[1].description)
        XCTAssertEqual(cursor, 0)
        XCTAssertEqual(page, 0)
        XCTAssertEqual(totalPages, 3)
        XCTAssertEqual(result.conversionMs, 11)
        XCTAssertEqual(result.pendingAsync, false)
    }

    func testDecodeCommitAndHide() throws {
        let json = """
            {"consumed":true,"actions":[{"text":"書きます","type":"commit"},{"type":"hide_candidates"},{"type":"hide_aux"}],"conversion_ms":0,"process_key_ms":0}
            """
        let result = try decodeKeyResult(json)
        XCTAssertEqual(result.actions.count, 3)
        guard case .commit(let text) = result.actions[0] else {
            return XCTFail("expected commit")
        }
        XCTAssertEqual(text, "書きます")
        guard case .hideCandidates = result.actions[1] else {
            return XCTFail("expected hide_candidates")
        }
        guard case .hideAux = result.actions[2] else {
            return XCTFail("expected hide_aux")
        }
    }

    func testDecodeInitResult() throws {
        let json = """
            {"protocol_version":1,"model_name":"jinen-v1-small-q5"}
            """
        let result = try makeProtocolDecoder().decode(InitResult.self, from: Data(json.utf8))
        XCTAssertEqual(result.protocolVersion, 1)
        XCTAssertEqual(result.modelName, "jinen-v1-small-q5")
    }

    func testUnknownActionTypeFails() {
        let json = """
            {"consumed":true,"actions":[{"type":"warp_to_mars"}],"conversion_ms":0,"process_key_ms":0}
            """
        XCTAssertThrowsError(try decodeKeyResult(json))
    }

    func testCounterCandidatePageIdentitySurvivesDelayedModelCompletion() throws {
        let json = """
            {"consumed":true,"actions":[{"candidates":[{"description":"件","text":"10件-09"},{"description":"件","text":"10件-10"}],"cursor":0,"page":1,"total_pages":2,"type":"show_candidates"}],"conversion_ms":20,"process_key_ms":0}
            """
        let result = try decodeKeyResult(json)
        guard
            case .showCandidates(let candidates, let cursor, let page, let totalPages) =
                result.actions[0]
        else {
            return XCTFail("expected show_candidates")
        }
        XCTAssertEqual(candidates.map(\.text), ["10件-09", "10件-10"])
        XCTAssertEqual(candidates.map(\.description), ["件", "件"])
        XCTAssertEqual(cursor, 0)
        XCTAssertEqual(page, 1)
        XCTAssertEqual(totalPages, 2)
        XCTAssertEqual(result.conversionMs, 20)
    }

    func testHomonymousCounterFirstPageOrderSurvivesDelayedModelCompletion() throws {
        let json = """
            {"consumed":true,"actions":[{"candidates":[{"text":"10件"},{"text":"10軒"}],"cursor":0,"page":0,"total_pages":2,"type":"show_candidates"}],"conversion_ms":20,"process_key_ms":0}
            """
        let result = try decodeKeyResult(json)
        guard
            case .showCandidates(let candidates, let cursor, let page, let totalPages) =
                result.actions[0]
        else {
            return XCTFail("expected show_candidates")
        }
        XCTAssertEqual(candidates.map(\.text), ["10件", "10軒"])
        XCTAssertEqual(cursor, 0)
        XCTAssertEqual(page, 0)
        XCTAssertEqual(totalPages, 2)
        XCTAssertEqual(result.conversionMs, 20)
    }
}
