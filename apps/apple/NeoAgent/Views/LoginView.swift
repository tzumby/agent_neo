import SwiftUI

#if os(iOS)
    import SafariServices
#endif

/// First launch: you sign in with Tailscale, the same account your desktop is
/// on, in an in-app browser (on the Mac, in the default browser). The app
/// becomes one of the devices on your tailnet; no Tailscale app needed.
struct LoginView: View {
    let loginURL: URL?
    @State private var showingLogin = false
    @Environment(\.openURL) private var openURL

    var body: some View {
        VStack(spacing: 18) {
            Spacer()
            Image(systemName: "display")
                .font(.system(size: 40, weight: .light))
                .frame(width: 96, height: 96)
                .glass(.circle)
            Text("Agent Neo")
                .font(.app(.largeTitle).bold())
            Text("Sign in with the Tailscale account your desktop is on.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
            Spacer()
            loginButton.primaryGlass().controlSize(.large)
            Text("This \(Device.noun) joins your tailnet as a device of its own.")
                .font(.app(.footnote))
                .foregroundStyle(.tertiary)
                .multilineTextAlignment(.center)
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
        #endif
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
                    Text("Log in with Tailscale").font(.app(.headline)).foregroundStyle(.black)
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 4)
        }
        .disabled(loginURL == nil)
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
