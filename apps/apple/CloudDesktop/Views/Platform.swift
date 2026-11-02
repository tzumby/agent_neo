import SwiftUI

#if os(iOS)
    import UIKit
#else
    import AppKit
#endif

// What the iPhone and the Mac do differently, in one place, so the screens
// themselves are shared. The Mac app also runs on macOS 15, before Liquid
// Glass: there, glass falls back to the system's materials and bordered buttons.

enum Clipboard {
    static func copy(_ text: String) {
        #if os(iOS)
            UIPasteboard.general.string = text
        #else
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
        #endif
    }

    /// Empties the clipboard (once a pasted key is saved).
    static func clear() {
        #if os(iOS)
            UIPasteboard.general.items = []
        #else
            NSPasteboard.general.clearContents()
        #endif
    }
}

enum Device {
    /// What the text calls this device.
    #if os(iOS)
        static let noun = "phone"
    #else
        static let noun = "Mac"
    #endif

    /// This device's name, as the desktop lists it for notifications.
    @MainActor static var name: String {
        #if os(iOS)
            UIDevice.current.name
        #else
            Host.current().localizedName ?? "Mac"
        #endif
    }
}

extension View {
    /// Liquid Glass, or a material before macOS 26.
    @ViewBuilder func glass<S: Shape>(_ shape: S, interactive: Bool = false) -> some View {
        #if os(iOS)
            glassEffect(interactive ? .regular.interactive() : .regular, in: shape)
        #else
            if #available(macOS 26, *) {
                glassEffect(interactive ? .regular.interactive() : .regular, in: shape)
            } else {
                background(.regularMaterial, in: shape)
                    .overlay(shape.stroke(Theme.hairline, lineWidth: 0.5))
            }
        #endif
    }

    /// A secondary button: glass, or bordered before macOS 26.
    @ViewBuilder func glassButton() -> some View {
        #if os(iOS)
            buttonStyle(.glass)
        #else
            if #available(macOS 26, *) { buttonStyle(.glass) } else { buttonStyle(CapsuleButtonStyle(prominent: false)) }
        #endif
    }

    /// The white button for the main action (its label is set black).
    @ViewBuilder func primaryGlass() -> some View {
        #if os(iOS)
            buttonStyle(.glassProminent).tint(.white)
        #else
            if #available(macOS 26, *) {
                buttonStyle(.glassProminent).tint(.white)
            } else {
                buttonStyle(CapsuleButtonStyle(prominent: true))
            }
        #endif
    }

    /// A bar over the content's top or bottom edge (the content scrolls under it).
    @ViewBuilder func bar<Content: View>(_ edge: VerticalEdge, @ViewBuilder content: () -> Content) -> some View {
        #if os(iOS)
            safeAreaBar(edge: edge) { content() }
        #else
            if #available(macOS 26, *) {
                safeAreaBar(edge: edge) { content() }
            } else {
                safeAreaInset(edge: edge) { content() }
            }
        #endif
    }

    /// A small title in the bar (the iPhone's default is a large one).
    func inlineTitle() -> some View {
        #if os(iOS)
            navigationBarTitleDisplayMode(.inline)
        #else
            self
        #endif
    }

    /// Right-click actions for list rows on the Mac, which has no swipe actions to find.
    func macContextMenu<Menu: View>(@ViewBuilder _ menu: () -> Menu) -> some View {
        #if os(macOS)
            contextMenu(menuItems: menu)
        #else
            self
        #endif
    }

    /// A sheet's size on the Mac, where sheets are as small as their content allows.
    func macSheetSize(width: CGFloat = 480, height: CGFloat = 560) -> some View {
        #if os(macOS)
            frame(minWidth: width, idealWidth: width, minHeight: height, idealHeight: height)
        #else
            self
        #endif
    }
}

#if os(macOS)
    /// Buttons before Liquid Glass, in the same colours: white for the main
    /// action, a faint white for the others. (The system's bordered buttons
    /// ignore a white tint.)
    private struct CapsuleButtonStyle: ButtonStyle {
        let prominent: Bool
        @Environment(\.isEnabled) private var isEnabled
        @Environment(\.controlSize) private var controlSize

        func makeBody(configuration: Configuration) -> some View {
            configuration.label
                .fontWeight(.medium)
                .foregroundStyle(prominent ? Color.black : Color.white)
                .padding(.horizontal, 14)
                .padding(.vertical, controlSize == .large ? 9 : 6)
                .background(prominent ? Color.white : Color.white.opacity(0.12), in: .capsule)
                .contentShape(.capsule)
                .opacity(isEnabled ? (configuration.isPressed ? 0.7 : 1) : 0.4)
        }
    }
#endif

/// Groups glass shapes so they blend; just the content before macOS 26.
struct GlassGroup<Content: View>: View {
    var spacing: CGFloat
    @ViewBuilder var content: Content

    var body: some View {
        #if os(iOS)
            GlassEffectContainer(spacing: spacing) { content }
        #else
            if #available(macOS 26, *) {
                GlassEffectContainer(spacing: spacing) { content }
            } else {
                content
            }
        #endif
    }
}

extension ToolbarItemPlacement {
    /// The bar's leading end: the iPhone's top left, the Mac window's toolbar start.
    static var barLeading: ToolbarItemPlacement {
        #if os(iOS)
            .topBarLeading
        #else
            .navigation
        #endif
    }

    static var barTrailing: ToolbarItemPlacement {
        #if os(iOS)
            .topBarTrailing
        #else
            .primaryAction
        #endif
    }
}

extension ToolbarPlacement {
    /// The iPhone's navigation bar, the Mac window's toolbar.
    static var appBar: ToolbarPlacement {
        #if os(iOS)
            .navigationBar
        #else
            .windowToolbar
        #endif
    }
}

/// A screen of sections: the iPhone's inset list, the Mac's grouped form
/// (as in System Settings: wrapped footers, chevrons on links).
struct GroupedList<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        #if os(iOS)
            List { content }
        #else
            Form { content }.formStyle(.grouped)
        #endif
    }
}

/// A form (editors): grouped on the Mac too.
struct GroupedForm<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        #if os(iOS)
            Form { content }
        #else
            Form { content }.formStyle(.grouped)
        #endif
    }
}
