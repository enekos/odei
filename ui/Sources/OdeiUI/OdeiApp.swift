import SwiftUI
import OdeiCore

@main
struct OdeiApp: App {
    @State private var agent = AgentSession()
    @State private var binary = Locator.binary
    @State private var workspace = Locator.workspace

    var body: some Scene {
        Window("odei", id: "main") {
            ContentView(agent: agent, binary: $binary, workspace: $workspace)
                .frame(minWidth: 720, minHeight: 460)
                .onAppear(perform: connect)
        }
        .defaultSize(width: 900, height: 680)
        .commands {
            CommandGroup(after: .newItem) {
                Button("New Session") { connect() }
                    .keyboardShortcut("n", modifiers: .command)
                Button("Open Workspace…") {
                    guard let picked = Locator.pickWorkspace() else { return }
                    workspace = picked
                    Locator.workspace = picked
                    connect()
                }
                .keyboardShortcut("o", modifiers: .command)
                Divider()
                Button("Interrupt") { agent.send(.cancel) }
                    .keyboardShortcut(".", modifiers: .command)
                    .disabled(!agent.running)
                Button("Compact Context") { agent.send(.compact) }
                    .disabled(agent.running)
            }
        }
    }

    private func connect() {
        guard let binary else { return }
        agent.start(binary: binary, workspace: workspace)
    }
}
