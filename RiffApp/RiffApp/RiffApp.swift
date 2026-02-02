import SwiftUI

@main
struct RiffApp: App {
    @StateObject private var serverManager = ServerManager()

    var body: some Scene {
        MenuBarExtra {
            MenuBarView(serverManager: serverManager)
        } label: {
            Image(systemName: serverManager.isRunning ? "waveform.circle.fill" : "waveform.circle")
        }

        Settings {
            PreferencesView(serverManager: serverManager)
        }
    }
}
