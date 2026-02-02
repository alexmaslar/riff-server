import Foundation
import Combine

@MainActor
class ServerManager: ObservableObject {
    @Published var isRunning = false
    @Published var lastOutput = ""

    private var process: Process?
    private var outputPipe: Pipe?

    var serverBinaryPath: String {
        // Look for riff-server binary next to the app bundle, or in a known build path
        let bundlePath = Bundle.main.bundlePath
        let appDir = (bundlePath as NSString).deletingLastPathComponent

        // Check for binary alongside the app
        let adjacentPath = (appDir as NSString).appendingPathComponent("riff-server")
        if FileManager.default.isExecutableFile(atPath: adjacentPath) {
            return adjacentPath
        }

        // Check in Resources
        if let resourcePath = Bundle.main.path(forResource: "riff-server", ofType: nil) {
            return resourcePath
        }

        // Fallback: cargo build output (development)
        let cargoDebugPath = (appDir as NSString)
            .deletingLastPathComponent
            .appending("/riff-server/target/debug/riff-server")
        if FileManager.default.isExecutableFile(atPath: cargoDebugPath) {
            return cargoDebugPath
        }

        return "riff-server"
    }

    func start() {
        guard !isRunning else { return }

        let process = Process()
        process.executableURL = URL(fileURLWithPath: serverBinaryPath)
        process.environment = ProcessInfo.processInfo.environment

        let pipe = Pipe()
        process.standardOutput = pipe
        process.standardError = pipe

        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            if let output = String(data: data, encoding: .utf8), !output.isEmpty {
                Task { @MainActor [weak self] in
                    self?.lastOutput = output
                }
            }
        }

        process.terminationHandler = { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.isRunning = false
                self?.process = nil
            }
        }

        do {
            try process.run()
            self.process = process
            self.outputPipe = pipe
            self.isRunning = true
        } catch {
            lastOutput = "Failed to start server: \(error.localizedDescription)"
        }
    }

    func stop() {
        guard isRunning, let process = process else { return }
        process.terminate()
        self.isRunning = false
        self.process = nil
    }

    func restart() {
        stop()
        // Brief delay before restarting
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in
            self?.start()
        }
    }
}
