import SwiftUI

struct MenuBarView: View {
    @ObservedObject var serverManager: ServerManager

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Circle()
                    .fill(serverManager.isRunning ? .green : .red)
                    .frame(width: 8, height: 8)
                Text(serverManager.isRunning ? "Server Running" : "Server Stopped")
                    .font(.headline)
            }

            Divider()

            if serverManager.isRunning {
                Button("Stop Server") {
                    serverManager.stop()
                }
                Button("Restart Server") {
                    serverManager.restart()
                }
            } else {
                Button("Start Server") {
                    serverManager.start()
                }
            }

            Divider()

            SettingsLink {
                Text("Preferences...")
            }

            Divider()

            Button("Quit Riff") {
                serverManager.stop()
                NSApplication.shared.terminate(nil)
            }
            .keyboardShortcut("q")
        }
        .padding(8)
    }
}
