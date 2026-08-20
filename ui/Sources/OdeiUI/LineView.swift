import SwiftUI
import OdeiCore

/// One transcript row. The vocabulary is the terminal's — `❯` for what you
/// typed, `⏺` for a group, `└` for a tool — so the two front ends read the
/// same way and the glyphs already mean something to anyone who has used
/// odei in a shell.
struct LineView: View {
    let line: Line
    let openCall: (Int) -> Void

    var body: some View {
        switch line.kind {
        case .user:
            HStack(alignment: .top, spacing: 10) {
                Text("❯")
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(.tertiary)
                Text(line.text)
                    .font(.system(size: 13))
                    .fontWeight(.medium)
            }
            .padding(.top, 6)

        case .assistant:
            Text(line.text)
                .font(.system(size: 13))
                .lineSpacing(2)
                .frame(maxWidth: .infinity, alignment: .leading)

        case .group:
            HStack(spacing: 8) {
                Text("⏺")
                    .font(.system(size: 9))
                Text(line.text)
                    .font(.system(size: 11, design: .monospaced))
            }
            .foregroundStyle(.secondary)
            .padding(.top, 4)

        case .tool:
            toolRow

        case .notice:
            Text(line.text)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(.tertiary)

        case .failure:
            Text(line.text)
                .font(.system(size: 12, design: .monospaced))
                .foregroundStyle(.red)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
                .background(Color.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 5))
        }
    }

    private var toolRow: some View {
        HStack(spacing: 8) {
            Text(line.lastInGroup ? "└" : "├")
                .foregroundStyle(.quaternary)
            if line.open {
                ProgressView().controlSize(.mini).scaleEffect(0.6).frame(width: 10)
            }
            Text(line.text)
                .foregroundStyle(line.isError ? Color.red : .secondary)
            if let call = line.call {
                // The handle is the whole affordance: it is what `/call N`
                // takes in the terminal, and clicking it opens the same report.
                Button("#\(call)") { openCall(call) }
                    .buttonStyle(.plain)
                    .foregroundStyle(.tertiary)
                    .help("Show call #\(call) in full")
            }
            Spacer(minLength: 0)
        }
        .font(.system(size: 11, design: .monospaced))
        .padding(.leading, 6)
    }
}

/// The blocking question. It sits above the composer rather than in a sheet:
/// deciding usually means reading the lines that led here, and a sheet hides
/// them.
struct ApprovalBar: View {
    let approval: PendingApproval
    let answer: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Image(systemName: "hand.raised.fill")
                    .foregroundStyle(.orange)
                Text(approval.label)
                    .font(.system(size: 12, weight: .medium))
                Text(approval.tool)
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(.secondary)
                Spacer()
            }
            ScrollView {
                Text(approval.detail)
                    .font(.system(size: 11, design: .monospaced))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 140)

            HStack(spacing: 8) {
                Button("Allow") { answer("allow") }
                    .keyboardShortcut("y", modifiers: [])
                Button("Always") { answer("always") }
                    .keyboardShortcut("a", modifiers: [])
                    .help("Remember this and stop asking")
                Button("Deny") { answer("deny") }
                    .keyboardShortcut("n", modifiers: [])
                Spacer()
                Text("y · a · n")
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(14)
        .background(Color.orange.opacity(0.07))
    }
}
