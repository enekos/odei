// Checks for the model layer, as a plain executable: `swift run OdeiChecks`.
//
// Not XCTest, because XCTest ships with Xcode and this app is built with only
// the Command Line Tools. The interesting cases are the ones a live model
// produces and a screenshot cannot prove: a record split across pipe reads, a
// tool `done` with no `start`, text resuming after tools.
import OdeiCore
import Foundation

@MainActor
var failures: [String] = []

@MainActor
func check(_ condition: Bool, _ label: String) {
    print(condition ? "  ok   \(label)" : "  FAIL \(label)")
    if !condition { failures.append(label) }
}

func event(_ json: String) -> AgentEvent {
    try! JSONDecoder().decode(AgentEvent.self, from: Data(json.utf8))
}

@MainActor
func session(_ lines: [String]) -> AgentSession {
    let session = AgentSession()
    for line in lines { session.handle(event(line)) }
    return session
}

@MainActor
func run() {
    print("--- transcript assembly ---")

    var s = session([
        #"{"event":"text","delta":"Let me "}"#,
        #"{"event":"text","delta":"look."}"#,
        #"{"event":"text_end"}"#,
    ])
    check(s.lines.count == 1 && s.lines[0].text == "Let me look." && !s.lines[0].open,
          "deltas accumulate into one closed assistant line")

    s = session([
        #"{"event":"text","delta":"First."}"#,
        #"{"event":"text_end"}"#,
        #"{"event":"tool","phase":"start","label":"Reading a.rs"}"#,
        #"{"event":"tool","phase":"done","label":"Read a.rs","error":false,"call":1}"#,
        #"{"event":"text","delta":"Second."}"#,
        #"{"event":"text_end"}"#,
    ])
    check(s.lines.filter { $0.kind == .assistant }.map(\.text) == ["First.", "Second."],
          "text after a tool starts a new line instead of appending")

    s = session([
        #"{"event":"group","summary":"1 tool call"}"#,
        #"{"event":"tool","phase":"start","label":"Running cargo test"}"#,
    ])
    check(s.lines.last!.open, "a started tool renders as running")
    s.handle(event(#"{"event":"tool","phase":"done","label":"Ran cargo test","error":true,"call":7}"#))
    let row = s.lines.last!
    check(s.lines.filter { $0.kind == .tool }.count == 1 && row.text == "Ran cargo test"
            && !row.open && row.isError && row.call == 7,
          "start and done are one row, carrying the error flag and #7")

    s = session([#"{"event":"tool","phase":"done","label":"Denied Running rm -rf /","error":true}"#])
    check(s.lines.count == 1 && s.lines[0].isError && s.lines[0].call == nil,
          "a denial arrives as a done with no start and still shows")

    s = session([
        #"{"event":"text","delta":""}"#,
        #"{"event":"text_end"}"#,
        #"{"event":"group","summary":"2 tool calls"}"#,
    ])
    check(s.lines.count == 1 && s.lines[0].kind == .group,
          "an empty text block leaves no blank row")

    print("--- state ---")

    s = session([
        #"{"event":"approval","id":3,"tool":"terminal","label":"Running rm -rf build","detail":"{}"}"#,
    ])
    check(s.approval?.id == 3 && s.approval?.tool == "terminal", "approval is raised")
    s.answer(s.approval!, "deny")
    check(s.approval == nil, "answering clears the approval")

    s = session([
        #"{"event":"waiting"}"#,
        #"{"event":"turn_end","ok":false,"error":"kimi request failed (HTTP 429)"}"#,
    ])
    check(!s.waiting && !s.running && s.lines.last?.kind == .failure
            && s.lines.last!.text.contains("429"),
          "a failed turn stops the spinner and shows the reason")

    s = session([
        #"{"event":"state","model":"k3","mode":"ask","session":"s1","context":0.42,"input_tokens":1000,"output_tokens":250,"workspace":"/tmp/w"}"#,
    ])
    check(s.model == "k3" && s.mode == "ask" && abs(s.contextFraction - 0.42) < 0.001
            && s.totalTokens == 1250,
          "state drives the status bar")

    s = session([
        #"{"event":"history","items":[{"role":"user","text":"fix the parser","tools":[]},{"role":"assistant","text":"Done.","tools":["Read src/parse.rs","Ran cargo test"]}]}"#,
    ])
    check(s.lines.map(\.kind) == [.user, .assistant, .tool, .tool]
            && s.lines[0].text == "fix the parser" && s.lines[3].text == "Ran cargo test",
          "a resumed session replays turns with their tools")

    s = session([#"{"event":"telemetry","whatever":1}"#])
    check(s.lines.isEmpty, "an unknown event from a newer odei is ignored, not fatal")

    print("--- pipe framing ---")

    let buffer = LineBuffer()
    check(buffer.take(Data(#"{"event":"a"}"#.utf8)).isEmpty, "a partial record is held back")
    let rejoined = buffer.take(Data("\n{\"event\":\"b\"}\n".utf8))
    check(rejoined.map { String(data: $0, encoding: .utf8) } == [#"{"event":"a"}"#, #"{"event":"b"}"#],
          "a record split across reads is rejoined")

    // A read can land mid-codepoint; splitting on bytes and decoding whole
    // lines is what keeps "⏺" from becoming replacement characters.
    let multibyte = LineBuffer()
    var bytes = Array(Data(#"{"event":"notice","text":"⏺ done"}"#.utf8))
    bytes.append(0x0a)
    let split = bytes.count - 8
    check(multibyte.take(Data(bytes[..<split])).isEmpty, "a multibyte split is held back")
    let whole = multibyte.take(Data(bytes[split...]))
    check(whole.count == 1 && event(String(data: whole[0], encoding: .utf8)!).text == "⏺ done",
          "a codepoint split across reads survives")

    check(LineBuffer().take(Data("{\"event\":\"a\"}\n{\"event\":\"b\"}\n{\"event\":\"c\"}\n".utf8)).count == 3,
          "several records in one read all come out")

    print("\n\(failures.count) failed")
    exit(failures.isEmpty ? 0 : 1)
}

MainActor.assumeIsolated { run() }
