import SwiftUI

struct PreferencesView: View {
    @ObservedObject var serverManager: ServerManager

    @AppStorage("libraryPath") private var libraryPath: String = ""
    @AppStorage("serverPort") private var serverPort: Int = 8080

    var body: some View {
        TabView {
            libraryTab
                .tabItem {
                    Label("Library", systemImage: "music.note.house")
                }

            serverTab
                .tabItem {
                    Label("Server", systemImage: "server.rack")
                }
        }
        .frame(width: 450, height: 300)
        .padding()
    }

    private var libraryTab: some View {
        Form {
            Section("Music Library") {
                HStack {
                    TextField("Library Path", text: $libraryPath)
                        .textFieldStyle(.roundedBorder)
                    Button("Browse...") {
                        selectFolder()
                    }
                }
            }
        }
    }

    private var serverTab: some View {
        Form {
            Section("Server") {
                HStack {
                    Text("Port:")
                    TextField("Port", value: $serverPort, format: .number)
                        .textFieldStyle(.roundedBorder)
                        .frame(width: 80)
                }

                HStack {
                    Circle()
                        .fill(serverManager.isRunning ? .green : .red)
                        .frame(width: 8, height: 8)
                    Text(serverManager.isRunning ? "Running" : "Stopped")
                }
            }

            Section("Server Output") {
                Text(serverManager.lastOutput)
                    .font(.system(.caption, design: .monospaced))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(4)
                    .background(Color(.textBackgroundColor))
                    .cornerRadius(4)
            }
        }
    }

    private func selectFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false

        if panel.runModal() == .OK, let url = panel.url {
            libraryPath = url.path
        }
    }
}
