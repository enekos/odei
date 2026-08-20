import SwiftUI
import OdeiCore

struct ContentView: View {
    @Bindable var agent: AgentSession
    @Binding var binary: URL?
    @Binding var workspace: URL

    @State private var draft = ""
    @State private var showInspector = false
    @FocusState private var composerFocused: Bool

    var body: some View {
        NavigationSplitView {
            Sidebar(agent: agent, showInspector: $showInspector, resume: resume)
                .navigationSplitViewColumnWidth(min: 190, ideal: 230, max: 320)
        } detail: {
            VStack(spacing: 0) {
                if let binary {
                    transcript
                    Divider()
                    if let approval = agent.approval {
                        ApprovalBar(approval: approval) { agent.answer(approval, $0) }
                        Divider()
                    }
                    composer
                    StatusBar(agent: agent, workspace: workspace, binary: binary)
                } else {
                    MissingBinary { picked in
                        binary = picked
                        Locator.binary = picked
                        agent.start(binary: picked, workspace: workspace)
                    }
                }
            }
        }
        .inspector(isPresented: $showInspector) {
            CallReport(agent: agent)
                .inspectorColumnWidth(min: 320, ideal: 460, max: 760)
        }
        .onChange(of: agent.callReport) { _, report in
            if report != nil { showInspector = true }
        }
        .navigationTitle(workspace.lastPathComponent)
        .navigationSubtitle(agent.model)
    }

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    ForEach(agent.lines) { line in
                        LineView(line: line) { agent.openCall($0) }
                            .id(line.id)
                    }
                    if agent.waiting { ThinkingRow() }
                    if let fatal = agent.fatal {
                        LineView(line: Line(kind: .failure, text: fatal)) { _ in }
                    }
                    // Anchor: scrolling to the last line stops short of it
                    // once the composer grows, so the tail gets its own.
                    Color.clear.frame(height: 1).id("bottom")
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 16)
                .textSelection(.enabled)
            }
            .onChange(of: agent.lines.count) { _, _ in scroll(proxy) }
            .onChange(of: agent.lines.last?.text) { _, _ in scroll(proxy) }
        }
    }

    private func scroll(_ proxy: ScrollViewProxy) {
        proxy.scrollTo("bottom", anchor: .bottom)
    }

    private var composer: some View {
        HStack(alignment: .bottom, spacing: 10) {
            Text("❯")
                .font(.system(.body, design: .monospaced))
                .foregroundStyle(.tertiary)
                .padding(.bottom, 6)
            TextField("Ask odei to do something…", text: $draft, axis: .vertical)
                .textFieldStyle(.plain)
                .font(.system(size: 13))
                .lineLimit(1...12)
                .focused($composerFocused)
                .disabled(agent.fatal != nil)
                .onSubmit(submit)
            if agent.running {
                Button("Stop", systemImage: "stop.fill") { agent.send(.cancel) }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .keyboardShortcut(".", modifiers: .command)
                    .help("Interrupt (⌘.)")
            } else {
                Button("Send", systemImage: "arrow.up.circle.fill") { submit() }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .help("Send (⌘↩)")
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
        .background(.background)
        .onAppear { composerFocused = true }
    }

    private func submit() {
        agent.submit(draft)
        draft = ""
        composerFocused = true
    }

    private func resume(_ id: String) {
        guard let binary else { return }
        agent.start(binary: binary, workspace: workspace, resume: id)
    }
}

// ------------------------------------------------------------------ sidebar

private struct Sidebar: View {
    @Bindable var agent: AgentSession
    @Binding var showInspector: Bool
    let resume: (String) -> Void

    var body: some View {
        List {
            if !agent.calls.isEmpty {
                Section("Calls") {
                    ForEach(agent.calls.reversed()) { call in
                        Button {
                            agent.openCall(call.n)
                            showInspector = true
                        } label: {
                            HStack(spacing: 6) {
                                Text("#\(call.n)")
                                    .font(.system(size: 10, design: .monospaced))
                                    .foregroundStyle(.tertiary)
                                Text(call.label)
                                    .lineLimit(1)
                                    .font(.system(size: 11))
                                    .foregroundStyle(call.isError ? Color.red : .primary)
                                Spacer(minLength: 0)
                            }
                        }
                        .buttonStyle(.plain)
                        .help("\(call.tool) · \(call.ms) ms · \(call.bytes) bytes")
                    }
                }
            }
            Section("Sessions") {
                ForEach(agent.sessions) { session in
                    Button { resume(session.id) } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(session.title)
                                .lineLimit(1)
                                .font(.system(size: 11))
                                .fontWeight(session.id == agent.sessionID ? .semibold : .regular)
                            Text("\(session.modified) · \(session.messages) msg")
                                .font(.system(size: 9))
                                .foregroundStyle(.tertiary)
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help(session.workspace)
                }
            }
        }
        .listStyle(.sidebar)
    }
}

// --------------------------------------------------------------- status bar

private struct StatusBar: View {
    @Bindable var agent: AgentSession
    let workspace: URL
    let binary: URL

    var body: some View {
        HStack(spacing: 12) {
            Menu(agent.mode.isEmpty ? "mode" : agent.mode) {
                ForEach(["ask", "auto", "yolo"], id: \.self) { mode in
                    Button(mode) { agent.send(.mode(mode)) }
                }
            }
            .menuStyle(.borderlessButton)
            .fixedSize()

            Menu(agent.model.isEmpty ? "model" : agent.model) {
                ForEach(agent.availableModels, id: \.id) { model in
                    Button("\(model.id) — \(model.note)") { agent.send(.model(model.id)) }
                }
            }
            .menuStyle(.borderlessButton)
            .fixedSize()

            Text(workspace.lastPathComponent)
                .lineLimit(1)
                .help(workspace.path)

            Spacer()

            if agent.totalTokens > 0 {
                Text("\(agent.totalTokens / 1000)k tok")
            }
            Text("ctx \(Int(agent.contextFraction * 100))%")
                .foregroundStyle(agent.contextFraction > 0.7 ? Color.orange : .secondary)
            Text("odei \(agent.version)")
                .help(binary.path)
        }
        .font(.system(size: 10, design: .monospaced))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 14)
        .padding(.vertical, 5)
        .background(.quaternary.opacity(0.35))
    }
}

// ------------------------------------------------------------------ pieces

private struct ThinkingRow: View {
    @State private var on = false

    var body: some View {
        HStack(spacing: 8) {
            Circle()
                .fill(.secondary)
                .frame(width: 6, height: 6)
                .opacity(on ? 1 : 0.25)
                .animation(.easeInOut(duration: 0.7).repeatForever(), value: on)
            Text("thinking")
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(.tertiary)
        }
        .onAppear { on = true }
    }
}

private struct MissingBinary: View {
    let picked: (URL) -> Void

    var body: some View {
        VStack(spacing: 14) {
            Text("odei not found")
                .font(.title3)
            Text("Install it with `cargo install --path .` in the odei repo, or point this app at the binary.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
            Button("Choose odei binary…") {
                if let url = Locator.pickBinary() { picked(url) }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private struct CallReport: View {
    @Bindable var agent: AgentSession

    var body: some View {
        Group {
            if let report = agent.callReport {
                ScrollView {
                    Text(report)
                        .font(.system(size: 11, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(14)
                }
                .navigationTitle(agent.callReportTitle)
            } else {
                Text("Pick a call to see exactly what it ran and everything it returned.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(30)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }
}
