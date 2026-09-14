// swift-tools-version:5.3
import PackageDescription

let package = Package(
    name: "tauri-plugin-ios-native",
    platforms: [.iOS(.v13)],
    products: [.library(name: "tauri-plugin-ios-native", type: .static, targets: ["tauri-plugin-ios-native"])],
    dependencies: [.package(name: "Tauri", path: "../.tauri/tauri-api")],
    targets: [.target(name: "tauri-plugin-ios-native", dependencies: [.byName(name: "Tauri")], path: "Sources")]
)
