// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "OdeiUI",
    platforms: [.macOS(.v14)],
    targets: [
        // The model layer: the protocol, the process, the transcript it
        // builds. Split out from the views so it can be checked without a
        // window — and, on a machine with only Command Line Tools, without
        // XCTest, which does not ship outside Xcode.
        .target(name: "OdeiCore", path: "Sources/OdeiCore"),
        .executableTarget(name: "OdeiUI", dependencies: ["OdeiCore"], path: "Sources/OdeiUI"),
        .executableTarget(name: "OdeiChecks", dependencies: ["OdeiCore"], path: "Sources/OdeiChecks"),
    ]
)
