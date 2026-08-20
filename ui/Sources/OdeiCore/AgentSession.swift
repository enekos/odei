import Foundation
import Observation

/// One line in the transcript. Assistant prose and running tool rows are
/// mutated in place as the stream arrives, which is why this is a struct with
/// a stable id rather than an enum of finished values.
public struct Line: Identifiable {
    public enum Kind {
        case user
        case assistant
        case group
        case tool
        case notice
        case failure
    }

    public let id = UUID()
    public var kind: Kind
    public var text: String
    /// Assistant text still being streamed, or a tool still running.
    public var open = false
    public var isError = false
    /// The `#N` handle this row can be reopened with, once it has finished.
    public var call: Int?
    /// Tool labels shown under a group summary get an indent glyph.
    public var lastInGroup = false

    public init(kind: Kind, text: String, open: Bool = false, isError: Bool = false,
                call: Int? = nil, lastInGroup: Bool = false) {
        self.kind = kind
        self.text = text
        self.open = open
        self.isError = isError
        self.call = call
        self.lastInGroup = lastInGroup
    }
}

/// Reassembles newline-delimited records from arbitrary pipe reads.
/// A reference type because `readabilityHandler` is an escaping closure and a
/// captured `var` there is a data race in the Swift 6 language mode; the
/// handler is serial, so the state itself needs no lock.
public final class LineBuffer: @unchecked Sendable {
    private var pending = Data()

    public init() {}

    public func take(_ chunk: Data) -> [Data] {
        pending.append(chunk)
        var lines: [Data] = []
        while let newline = pending.firstIndex(of: 0x0a) {
            let line = pending[pending.startIndex..<newline]
            pending.removeSubrange(pending.startIndex...newline)
            if !line.isEmpty { lines.append(Data(line)) }
        }
        return lines
    }
}

public struct PendingApproval: Identifiable {
    public let id: Int
    public let tool: String
    public let label: String
    public let detail: String
}

public struct SessionRow: Identifiable, Hashable {
    public let id: String
    public let title: String
    public let workspace: String
    public let messages: Int
    public let modified: String
}

public struct CallRow: Identifiable, Hashable {
    public var id: Int { n }
    public let n: Int
    public let tool: String
    public let label: String
    public let ms: Int
    public let isError: Bool
    public let bytes: Int
}

/// Drives one `odei serve` process: spawns it, decodes its event stream, and
/// keeps the state the views read. Everything here is main-actor; the only
/// work off it is reading bytes from the pipe.
@MainActor
@Observable
public final class AgentSession {
    public private(set) var lines: [Line] = []
    public private(set) var running = false
    public private(set) var waiting = false
    public private(set) var approval: PendingApproval?
    public private(set) var sessions: [SessionRow] = []
    public private(set) var calls: [CallRow] = []
    public private(set) var callReport: String?
    public private(set) var callReportTitle = ""

    public private(set) var model = ""
    public private(set) var mode = ""
    public private(set) var workspace = ""
    public private(set) var sessionID = ""
    public private(set) var version = ""
    public private(set) var contextFraction = 0.0
    public private(set) var totalTokens = 0
    public private(set) var availableModels: [AgentEvent.ModelInfo] = []
    /// Set when the process could not start or died on us; blocks the composer.
    public private(set) var fatal: String?

    private var process: Process?
    private var stdin: FileHandle?
    /// Index of the tool row awaiting its `done` event. Tools in a group run
    /// one at a time, so the pending row is always the most recent one.
    private var openToolIndex: Int?

    public var isConnected: Bool { process?.isRunning ?? false }

    // ------------------------------------------------------------ lifecycle

    public init() {}

    public func start(binary: URL, workspace: URL, resume: String? = nil) {
        stop()
        lines.removeAll()
        calls.removeAll()
        callReport = nil
        approval = nil
        fatal = nil
        openToolIndex = nil
        self.workspace = workspace.path

        let process = Process()
        process.executableURL = binary
        var arguments = ["serve", "--workspace", workspace.path]
        if let resume { arguments += ["--resume", resume] }
        process.arguments = arguments
        process.currentDirectoryURL = workspace
        // A GUI app inherits launchd's environment, not a shell's, so the
        // agent would otherwise run without a usable PATH for `terminal`.
        var environment = ProcessInfo.processInfo.environment
        let path = environment["PATH"] ?? ""
        if !path.contains("/usr/local/bin") {
            environment["PATH"] = "/opt/homebrew/bin:/usr/local/bin:" + path
        }
        process.environment = environment

        let outPipe = Pipe()
        let inPipe = Pipe()
        let errPipe = Pipe()
        process.standardOutput = outPipe
        process.standardInput = inPipe
        process.standardError = errPipe

        // A read can split a JSON line anywhere, including mid-multibyte, so
        // the tail is held until its newline arrives.
        let buffer = LineBuffer()
        outPipe.fileHandleForReading.readabilityHandler = { handle in
            let chunk = handle.availableData
            if chunk.isEmpty { return }
            let events = buffer.take(chunk).compactMap {
                try? JSONDecoder().decode(AgentEvent.self, from: $0)
            }
            if events.isEmpty { return }
            Task { @MainActor [weak self] in
                for event in events { self?.handle(event) }
            }
        }
        // stderr is odei's own diagnostics, never protocol — surface it as a
        // failure line rather than letting the pipe fill and stall the child.
        errPipe.fileHandleForReading.readabilityHandler = { handle in
            let chunk = handle.availableData
            guard !chunk.isEmpty, let text = String(data: chunk, encoding: .utf8) else { return }
            Task { @MainActor [weak self] in
                self?.append(.failure, text.trimmingCharacters(in: .whitespacesAndNewlines))
            }
        }
        process.terminationHandler = { _ in
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.running = false
                self.waiting = false
                if self.fatal == nil { self.fatal = "odei exited" }
            }
        }

        do {
            try process.run()
        } catch {
            fatal = "could not start \(binary.path): \(error.localizedDescription)"
            return
        }
        self.process = process
        self.stdin = inPipe.fileHandleForWriting
        send(.sessions)
    }

    public func stop() {
        guard let process else { return }
        send(.exit)
        self.process = nil
        self.stdin = nil
        // Give it a moment to unwind on its own before insisting.
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.4) {
            if process.isRunning { process.terminate() }
        }
    }

    // ------------------------------------------------------------- commands

    public func send(_ command: AgentCommand) {
        guard let stdin,
              let data = try? JSONSerialization.data(withJSONObject: command.json)
        else { return }
        var line = data
        line.append(0x0a)
        // A dead pipe throws SIGPIPE-as-exception; the child is simply gone.
        do { try stdin.write(contentsOf: line) } catch { fatal = "odei is not listening" }
    }

    public func submit(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !running, fatal == nil else { return }
        append(.user, trimmed)
        running = true
        waiting = true
        send(.prompt(trimmed))
    }

    public func answer(_ approval: PendingApproval, _ answer: String) {
        self.approval = nil
        send(.approve(id: approval.id, answer: answer))
    }

    public func openCall(_ n: Int) {
        send(.call(n))
    }

    public func closeCall() {
        callReport = nil
    }

    // -------------------------------------------------------------- events

    /// Internal rather than private so the tests can feed it a stream without
    /// spawning a process.
    public func handle(_ event: AgentEvent) {
        switch event.event {
        case "ready":
            version = event.version ?? ""
            availableModels = event.models ?? []

        case "state":
            model = event.model ?? model
            mode = event.mode ?? mode
            workspace = event.workspace ?? workspace
            sessionID = event.session ?? sessionID
            contextFraction = event.context ?? contextFraction
            totalTokens = (event.inputTokens ?? 0) + (event.outputTokens ?? 0)

        case "history":
            for item in event.items ?? [] {
                let text = item.text ?? ""
                if !text.isEmpty {
                    append(item.role == "user" ? .user : .assistant, text)
                }
                for label in item.tools ?? [] {
                    lines.append(Line(kind: .tool, text: label, lastInGroup: true))
                }
            }

        case "waiting":
            waiting = true

        case "text":
            waiting = false
            appendDelta(event.delta ?? "")

        case "text_end":
            closeOpenAssistant()

        case "group":
            waiting = false
            append(.group, event.summary ?? "")

        case "tool":
            waiting = false
            if event.phase == "start" {
                lines.append(Line(kind: .tool, text: event.label ?? "", open: true,
                                  lastInGroup: true))
                openToolIndex = lines.count - 1
            } else {
                let index = openToolIndex ?? lines.indices.last(where: { lines[$0].kind == .tool })
                if let index, lines.indices.contains(index) {
                    lines[index].text = event.label ?? lines[index].text
                    lines[index].open = false
                    lines[index].isError = event.error ?? false
                    lines[index].call = event.call
                } else {
                    // A denial reports `done` without a matching `start`.
                    lines.append(Line(kind: .tool, text: event.label ?? "",
                                      isError: event.error ?? false, call: event.call,
                                      lastInGroup: true))
                }
                openToolIndex = nil
            }

        case "approval":
            waiting = false
            approval = PendingApproval(id: event.id ?? 0, tool: event.tool ?? "",
                                       label: event.label ?? "", detail: event.detail ?? "")

        case "notice":
            append(.notice, event.text ?? "")

        case "turn_end":
            closeOpenAssistant()
            running = false
            waiting = false
            if event.ok != true, let error = event.errorText {
                append(.failure, error)
            }
            send(.calls)

        case "sessions":
            sessions = (event.items ?? []).map {
                SessionRow(id: $0.id ?? "", title: $0.title ?? "untitled",
                           workspace: $0.workspace ?? "", messages: $0.messages ?? 0,
                           modified: $0.modified ?? "")
            }

        case "calls":
            calls = (event.items ?? []).map {
                CallRow(n: $0.n ?? 0, tool: $0.tool ?? "", label: $0.label ?? "",
                        ms: $0.ms ?? 0, isError: $0.error ?? false, bytes: $0.bytes ?? 0)
            }

        case "call":
            callReportTitle = "#\(event.n ?? 0)  \(event.label ?? "")"
            callReport = event.report

        case "error":
            append(.failure, event.text ?? "")

        case "fatal":
            fatal = event.text ?? "odei could not start"
            running = false
            waiting = false

        default:
            break
        }
    }

    private func append(_ kind: Line.Kind, _ text: String) {
        guard !text.isEmpty else { return }
        closeOpenAssistant()
        lines.append(Line(kind: kind, text: text))
    }

    private func appendDelta(_ delta: String) {
        if let index = lines.indices.last, lines[index].kind == .assistant, lines[index].open {
            lines[index].text += delta
        } else {
            lines.append(Line(kind: .assistant, text: delta, open: true))
        }
    }

    private func closeOpenAssistant() {
        if let index = lines.indices.last, lines[index].kind == .assistant, lines[index].open {
            lines[index].open = false
            if lines[index].text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                lines.removeLast()
            }
        }
    }
}
