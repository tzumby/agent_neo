import SafariServices
import SwiftUI

/// First launch: sign in with Tailscale in an in-app browser. The app becomes
/// one of your devices on the tailnet; no Tailscale app needed.
struct LoginView: View {
    let loginURL: URL?
    @State private var showingLogin = false

    var body: some View {
        VStack(spacing: 24) {
            Spacer()
            Image(systemName: "desktopcomputer.and.macbook")
                .font(.system(size: 64))
                .foregroundStyle(.tint)
            Text("Cloud Desktop")
                .font(.largeTitle.bold())
            Text("Sign in with Tailscale to reach your desktop. This app joins your tailnet as one of your devices.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
            Spacer()
            Button {
                showingLogin = true
            } label: {
                if loginURL == nil {
                    ProgressView().frame(maxWidth: .infinity)
                } else {
                    Text("Log in with Tailscale").frame(maxWidth: .infinity)
                }
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .disabled(loginURL == nil)
        }
        .padding(32)
        // Closes by itself: once you've signed in, the root view moves on.
        .sheet(isPresented: $showingLogin) {
            if let loginURL {
                SafariView(url: loginURL).ignoresSafeArea()
            }
        }
    }
}

struct SafariView: UIViewControllerRepresentable {
    let url: URL

    func makeUIViewController(context: Context) -> SFSafariViewController {
        SFSafariViewController(url: url)
    }

    func updateUIViewController(_ controller: SFSafariViewController, context: Context) {}
}
