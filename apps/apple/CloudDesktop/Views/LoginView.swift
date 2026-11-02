import SwiftUI
import Tailnet
import UniformTypeIdentifiers

#if os(iOS)
    import PhotosUI
    import SafariServices
#endif

/// First launch. A tester (no Tailscale account) uses the setup code they
/// were sent: scanned with the camera (iPhone), read from a picture of it,
/// tapped or pasted as a link. You sign in with Tailscale in an in-app browser
/// (on the Mac, in the default browser). Either way the app becomes one of the
/// devices on the tailnet; no Tailscale app needed.
struct LoginView: View {
    let loginURL: URL?
    @State private var showingLogin = false
    @State private var pasting = false
    @State private var pasted = ""
    @State private var problem: String?
    #if os(iOS)
        @State private var scanning = false
        @State private var picture: PhotosPickerItem?
    #else
        @State private var choosingPicture = false
    #endif
    @Environment(\.openURL) private var openURL
    @Environment(\.useSetupLink) private var useSetupLink

    var body: some View {
        VStack(spacing: 18) {
            Spacer()
            Image(systemName: "display")
                .font(.system(size: 40, weight: .light))
                .frame(width: 96, height: 96)
                .glass(.circle)
            Text("Cloud Desktop")
                .font(.app(.largeTitle).bold())
            Text(intro)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
            Spacer()
            if pasting { pasteField }
            #if os(iOS)
                Button {
                    problem = nil
                    scanning = true
                } label: {
                    Label("Scan setup code", systemImage: "qrcode.viewfinder")
                        .font(.app(.headline))
                        .foregroundStyle(.black)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 4)
                }
                .primaryGlass()
                .controlSize(.large)
                otherWays
                loginButton.glassButton().controlSize(.large)
            #else
                otherWays
                loginButton.primaryGlass().controlSize(.large)
            #endif
            if let problem {
                Text(problem).font(.app(.footnote)).foregroundStyle(.orange).multilineTextAlignment(.center)
            }
        }
        .padding(28)
        .frame(maxWidth: 480)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black)
        #if os(iOS)
            // Closes by itself: once you've signed in, the root view moves on.
            .sheet(isPresented: $showingLogin) {
                if let loginURL {
                    SafariView(url: loginURL).ignoresSafeArea()
                }
            }
            .fullScreenCover(isPresented: $scanning) {
                SetupCodeScanner { key in useSetupLink?.use(key) }
            }
            .onChange(of: picture) {
                guard let item = picture else { return }
                picture = nil
                Task {
                    let data = try? await item.loadTransferable(type: Data.self)
                    await read(data)
                }
            }
        #else
            .fileImporter(isPresented: $choosingPicture, allowedContentTypes: [.image]) { result in
                guard case .success(let url) = result else { return }
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                let data = try? Data(contentsOf: url)
                Task { await read(data) }
            }
        #endif
    }

    private var intro: String {
        #if os(iOS)
            "Scan the setup code you were sent, or sign in with Tailscale to reach your desktop."
        #else
            "Open the setup link you were sent, or sign in with Tailscale to reach your desktop."
        #endif
    }

    /// A picture of the code (a screenshot, a saved image), or the link pasted.
    private var otherWays: some View {
        HStack(spacing: 14) {
            #if os(iOS)
                PhotosPicker(selection: $picture, matching: .images) { Text("Use a picture of the code") }
            #else
                Button("Use a picture of the code…") {
                    problem = nil
                    choosingPicture = true
                }
            #endif
            Text("·").foregroundStyle(.tertiary)
            Button("Paste the link") {
                problem = nil
                pasting = true
            }
        }
        .font(.app(.subheadline).weight(.medium))
        .buttonStyle(.plain)
        .foregroundStyle(.secondary)
    }

    private var pasteField: some View {
        VStack(spacing: 8) {
            HStack(spacing: 10) {
                TextField("Paste your setup link", text: $pasted)
                    .textFieldStyle(.plain)
                    .autocorrectionDisabled()
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .glass(.capsule)
                    .onSubmit(usePasted)
                Button("Connect", action: usePasted)
                    .glassButton()
                    .disabled(SetupLink.authKey(in: pasted) == nil)
            }
            if !pasted.isEmpty, SetupLink.authKey(in: pasted) == nil {
                Text("That isn't a setup link: it starts with clouddesktop://join")
                    .font(.app(.footnote))
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var loginButton: some View {
        Button {
            #if os(iOS)
                showingLogin = true
            #else
                if let loginURL { openURL(loginURL) }
            #endif
        } label: {
            Group {
                if loginURL == nil {
                    ProgressView()
                } else {
                    #if os(iOS)
                        Text("Log in with Tailscale").font(.app(.headline))
                    #else
                        Text("Log in with Tailscale").font(.app(.headline)).foregroundStyle(.black)
                    #endif
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 4)
        }
        .disabled(loginURL == nil)
    }

    private func usePasted() {
        guard let key = SetupLink.authKey(in: pasted) else { return }
        pasted = ""
        pasting = false
        useSetupLink?.use(key)
    }

    /// Reads the setup code out of a picture, off the main thread.
    private func read(_ data: Data?) async {
        guard let data else {
            problem = "Couldn't open that picture."
            return
        }
        let key = await Task.detached { SetupLink.authKey(inPicture: data) }.value
        if let key {
            problem = nil
            useSetupLink?.use(key)
        } else {
            problem = "No setup code in that picture. Use the one you were sent, with the whole code in view."
        }
    }
}

#if os(iOS)
struct SafariView: UIViewControllerRepresentable {
    let url: URL

    func makeUIViewController(context: Context) -> SFSafariViewController {
        SFSafariViewController(url: url)
    }

    func updateUIViewController(_ controller: SFSafariViewController, context: Context) {}
}
#endif
