import Foundation
import OsmTileCompilerFFI

/// A job that failed or could not start: the C ABI's `{kind, message, stage}`.
struct JobFailure: Error, LocalizedError {
    var kind: String
    var message: String
    var stage: String?

    init(kind: String, message: String, stage: String? = nil) {
        self.kind = kind
        self.message = message
        self.stage = stage
    }

    init(json: [String: Any]?) {
        kind = json?["kind"] as? String ?? "internal"
        message = json?["message"] as? String ?? "The library returned no error details"
        stage = json?["stage"] as? String
    }

    var errorDescription: String? {
        if let stage { return "\(message) [\(kind), \(stage)]" }
        return "\(message) [\(kind)]"
    }

    var json: [String: Any] {
        var out: [String: Any] = ["kind": kind, "message": message]
        if let stage { out["stage"] = stage }
        return out
    }
}

/// `otc_job_poll`'s progress JSON.
struct JobProgress: Decodable, Equatable {
    var state: String
    var kind: String
    var cancelRequested: Bool
    var updates: Int
    var stage: String?
    var message: String
    var fraction: Double?
    var overall: Double?
}

enum CompilerLibrary {
    static var version: String { String(cString: otc_version()) }
    static var abiVersion: UInt32 { otc_abi_version() }
    static var headerAbiVersion: UInt32 { UInt32(OTC_ABI_VERSION) }
}

/// One C ABI job. The C ABI lets every call but free come from any thread.
final class CompilerJob: @unchecked Sendable {
    private let handle: OpaquePointer
    let kind: String

    init(kind: String, request: [String: Any]) throws {
        self.kind = kind
        let envelope: [String: Any] = [
            "abiVersion": Int(OTC_ABI_VERSION),
            "kind": kind,
            "request": request,
        ]
        let data = try JSONSerialization.data(withJSONObject: envelope, options: [.withoutEscapingSlashes])
        let json = String(decoding: data, as: UTF8.self)
        var job: OpaquePointer?
        var error: UnsafeMutablePointer<CChar>?
        let status = otc_job_start(json, &job, &error)
        let errorJSON = takeString(error)
        guard status == OTC_OK, let job else {
            throw JobFailure(json: errorJSON.flatMap(parseObject))
        }
        handle = job
    }

    deinit { otc_job_free(handle) }

    /// Blocks up to `milliseconds`; returns an `OTC_JOB_*` state.
    func wait(milliseconds: UInt32) -> Int32 { otc_job_wait(handle, milliseconds) }

    var state: Int32 { otc_job_state(handle) }

    func poll() -> JobProgress? {
        guard let text = takeString(otc_job_poll(handle)) else { return nil }
        return try? JSONDecoder().decode(JobProgress.self, from: Data(text.utf8))
    }

    func cancel() { otc_job_cancel(handle) }

    /// The job's result object, or its error thrown as a `JobFailure`.
    func result() throws -> [String: Any] {
        var out: UnsafeMutablePointer<CChar>?
        let status = otc_job_result(handle, &out)
        let text = takeString(out)
        guard status == OTC_OK, let text, let object = parseObject(text) else {
            throw JobFailure(kind: "internal", message: "otc_job_result returned status \(status)")
        }
        if object["ok"] as? Bool == true {
            return object["result"] as? [String: Any] ?? [:]
        }
        throw JobFailure(json: object["error"] as? [String: Any])
    }
}

private func takeString(_ pointer: UnsafeMutablePointer<CChar>?) -> String? {
    guard let pointer else { return nil }
    defer { otc_string_free(pointer) }
    return String(cString: pointer)
}

func parseObject(_ text: String) -> [String: Any]? {
    (try? JSONSerialization.jsonObject(with: Data(text.utf8))) as? [String: Any]
}
