import Foundation

/// One line from `odei serve`. The wire format is a tagged union, but the
/// tags share most of their fields, so a single struct of optionals decodes
/// every case without a per-event type — and, more usefully, tolerates a
/// newer odei adding fields this build has never heard of.
public struct AgentEvent: Decodable {
    public var event: String
    public var text: String?
    public var delta: String?

    // ready
    public var version: String?
    public var keySource: String?
    public var contextWindow: Int?
    public var models: [ModelInfo]?

    // state
    public var session: String?
    public var workspace: String?
    public var model: String?
    public var mode: String?
    public var context: Double?
    public var inputTokens: Int?
    public var outputTokens: Int?
    public var turns: Int?

    // tools and groups
    public var summary: String?
    public var phase: String?
    public var label: String?
    public var error: Bool?
    public var call: Int?

    // approval
    public var id: Int?
    public var tool: String?
    public var detail: String?

    // turn_end
    public var ok: Bool?
    public var errorText: String?

    // lists
    public var items: [Item]?
    public var report: String?
    public var n: Int?

    public struct ModelInfo: Decodable, Hashable {
        public var id: String
        public var note: String
    }

    /// Rows of `history`, `sessions`, and `calls`. Same reasoning as above.
    public struct Item: Decodable, Hashable {
        public var role: String?
        public var text: String?
        public var tools: [String]?

        public var id: String?
        public var title: String?
        public var workspace: String?
        public var messages: Int?
        public var modified: String?

        public var n: Int?
        public var tool: String?
        public var label: String?
        public var ms: Int?
        public var error: Bool?
        public var at: String?
        public var bytes: Int?
    }

    private enum CodingKeys: String, CodingKey {
        case event, text, delta, version, models, session, workspace, model, mode, context
        case summary, phase, label, error, call, id, tool, detail, ok, items, report, turns, n
        case keySource = "key_source"
        case contextWindow = "context_window"
        case inputTokens = "input_tokens"
        case outputTokens = "output_tokens"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        event = try c.decode(String.self, forKey: .event)
        text = try? c.decode(String.self, forKey: .text)
        delta = try? c.decode(String.self, forKey: .delta)
        version = try? c.decode(String.self, forKey: .version)
        keySource = try? c.decode(String.self, forKey: .keySource)
        contextWindow = try? c.decode(Int.self, forKey: .contextWindow)
        models = try? c.decode([ModelInfo].self, forKey: .models)
        session = try? c.decode(String.self, forKey: .session)
        workspace = try? c.decode(String.self, forKey: .workspace)
        model = try? c.decode(String.self, forKey: .model)
        mode = try? c.decode(String.self, forKey: .mode)
        context = try? c.decode(Double.self, forKey: .context)
        inputTokens = try? c.decode(Int.self, forKey: .inputTokens)
        outputTokens = try? c.decode(Int.self, forKey: .outputTokens)
        turns = try? c.decode(Int.self, forKey: .turns)
        summary = try? c.decode(String.self, forKey: .summary)
        phase = try? c.decode(String.self, forKey: .phase)
        label = try? c.decode(String.self, forKey: .label)
        // `error` is a bool on tool lines and a string on turn_end.
        error = try? c.decode(Bool.self, forKey: .error)
        errorText = try? c.decode(String.self, forKey: .error)
        call = try? c.decode(Int.self, forKey: .call)
        id = try? c.decode(Int.self, forKey: .id)
        tool = try? c.decode(String.self, forKey: .tool)
        detail = try? c.decode(String.self, forKey: .detail)
        ok = try? c.decode(Bool.self, forKey: .ok)
        items = try? c.decode([Item].self, forKey: .items)
        report = try? c.decode(String.self, forKey: .report)
        n = try? c.decode(Int.self, forKey: .n)
    }
}

/// A line written back to `odei serve`.
public enum AgentCommand {
    case prompt(String)
    case approve(id: Int, answer: String)
    case cancel
    case compact
    case sessions
    case calls
    case call(Int)
    case model(String)
    case mode(String)
    case exit

    public var json: [String: Any] {
        switch self {
        case .prompt(let text): return ["cmd": "prompt", "text": text]
        case .approve(let id, let answer): return ["cmd": "approve", "id": id, "answer": answer]
        case .cancel: return ["cmd": "cancel"]
        case .compact: return ["cmd": "compact"]
        case .sessions: return ["cmd": "sessions"]
        case .calls: return ["cmd": "calls"]
        case .call(let n): return ["cmd": "call", "n": n]
        case .model(let value): return ["cmd": "model", "value": value]
        case .mode(let value): return ["cmd": "mode", "value": value]
        case .exit: return ["cmd": "exit"]
        }
    }
}
