import Foundation
import AppKit

/// Where the `odei` binary and the workspace live, and how to remember them.
///
/// A GUI app is launched by launchd, so it has none of the shell's PATH and
/// cannot just run `odei`. The binary is looked for in the usual install
/// locations, and whatever is found — or picked by hand — is remembered.
public enum Locator {
    private static let binaryKey = "odeiBinaryPath"
    private static let workspaceKey = "odeiWorkspacePath"

    public static var binary: URL? {
        get {
            if let override = ProcessInfo.processInfo.environment["ODEI_BIN"] {
                return URL(fileURLWithPath: override)
            }
            if let saved = UserDefaults.standard.string(forKey: binaryKey),
               FileManager.default.isExecutableFile(atPath: saved) {
                return URL(fileURLWithPath: saved)
            }
            return candidates.first { FileManager.default.isExecutableFile(atPath: $0) }
                .map { URL(fileURLWithPath: $0) }
        }
        set { UserDefaults.standard.set(newValue?.path, forKey: binaryKey) }
    }

    private static var candidates: [String] {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return [
            "\(home)/.cargo/bin/odei",
            "\(home)/.local/bin/odei",
            "/opt/homebrew/bin/odei",
            "/usr/local/bin/odei",
        ]
    }

    public static var workspace: URL {
        get {
            if let saved = UserDefaults.standard.string(forKey: workspaceKey),
               FileManager.default.fileExists(atPath: saved) {
                return URL(fileURLWithPath: saved)
            }
            return FileManager.default.homeDirectoryForCurrentUser
        }
        set { UserDefaults.standard.set(newValue.path, forKey: workspaceKey) }
    }

    @MainActor
    public static func pickWorkspace() -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.directoryURL = workspace
        panel.prompt = "Open"
        panel.message = "Choose the folder odei should work in."
        return panel.runModal() == .OK ? panel.url : nil
    }

    @MainActor
    public static func pickBinary() -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Use"
        panel.message = "Choose the odei binary (cargo install --path . puts it in ~/.cargo/bin)."
        return panel.runModal() == .OK ? panel.url : nil
    }
}
