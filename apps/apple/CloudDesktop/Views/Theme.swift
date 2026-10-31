import AgentAPI
import ChatCore
import SwiftUI

/// Grok-style look: black, a few greys, white for the main action, and
/// Liquid Glass only on things that float above the content (bars, the
/// composer, buttons), never on the content itself.
enum Theme {
    static let surface = Color(white: 0.11)  // cards
    static let inset = Color(white: 0.06)  // code inside cards
    static let userBubble = Color(white: 0.17)
    static let hairline = Color.white.opacity(0.08)
}

extension View {
    /// A content card: dark surface with a hairline edge.
    func card() -> some View {
        padding(16)
            .background(Theme.surface, in: .rect(cornerRadius: 24))
            .overlay(RoundedRectangle(cornerRadius: 24).strokeBorder(Theme.hairline, lineWidth: 0.5))
    }

    /// The white glass button for the main action (its label is set black).
    func primaryGlass() -> some View {
        buttonStyle(.glassProminent).tint(.white)
    }
}

/// Opens the take-over view for an Action needed card.
struct TakeOverAction: Sendable, Equatable {
    let open: @MainActor @Sendable (Action) -> Void

    /// Always equal: the closure is rebuilt on every chat update but does the
    /// same thing, and an unequal environment value would redraw every card.
    static func == (lhs: TakeOverAction, rhs: TakeOverAction) -> Bool { true }
}

// Chat rows get the store, client and take-over action from the environment,
// so each row's only input is its (Equatable) item and SwiftUI redraws a row
// only when that item changes.
extension EnvironmentValues {
    @Entry var chatStore: ChatStore? = nil
    @Entry var agentClient: AgentClient? = nil
    @Entry var takeOver: TakeOverAction? = nil
}
