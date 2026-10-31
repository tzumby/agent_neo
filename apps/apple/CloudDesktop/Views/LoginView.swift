import SafariServices
import SwiftUI

/// First launch: sign in with Tailscale in an in-app browser. The app becomes
/// one of your devices on the tailnet; no Tailscale app needed.
struct LoginView: View {
    let loginURL: URL?
    @State private var showingLogin = false

    var body: some View {
        VStack(spacing: 18) {
            Spacer()
            Image(systemName: "display")
                .font(.system(size: 40, weight: .light))
                .frame(width: 96, height: 96)
                .glassEffect(.regular, in: .circle)
            Text("Cloud Desktop")
                .font(.largeTitle.bold())
            Text("Sign in with Tailscale to reach your desktop. This app joins your tailnet as one of your devices.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
            Spacer()
            Button {
                showingLogin = true
            } label: {
                Group {
                    if loginURL == nil {
                        ProgressView().tint(.black)
                    } else {
                        Text("Log in with Tailscale").font(.headline).foregroundStyle(.black)
                    }
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 4)
            }
            .primaryGlass()
            .controlSize(.large)
            .disabled(loginURL == nil)
        }
        .padding(28)
        .background(Color.black)
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
