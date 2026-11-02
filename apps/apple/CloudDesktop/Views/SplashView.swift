import SwiftUI

/// The loading screen for every step of getting to the chat (Tailscale, the
/// desktop, the first sync), so they read as one: the app's mark on black and
/// a line saying what's happening.
struct SplashView: View {
    let status: String
    var detail: String?
    @State private var breathing = false

    var body: some View {
        VStack(spacing: 18) {
            Image(systemName: "display")
                .font(.system(size: 38, weight: .light))
                .frame(width: 92, height: 92)
                .glass(.circle)
                .scaleEffect(breathing ? 1.04 : 0.97)
                .animation(.easeInOut(duration: 1.2).repeatForever(autoreverses: true), value: breathing)
            Text("Cloud Desktop")
                .font(.title3.weight(.semibold))
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text(status)
            }
            .font(.subheadline)
            .foregroundStyle(.secondary)
            .contentTransition(.opacity)
            .animation(.easeOut(duration: 0.2), value: status)
            if let detail {
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.tertiary)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 40)
            }
        }
        // Same spot on every step: centred on the whole screen.
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black)
        .ignoresSafeArea()
        .toolbarVisibility(.hidden, for: .appBar)
        .onAppear { breathing = true }
    }
}
