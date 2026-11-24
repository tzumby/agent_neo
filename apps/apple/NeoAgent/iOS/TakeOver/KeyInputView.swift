import DesktopViewer
import UIKit

/// An invisible view that takes the iPhone keyboard and types on the desktop.
/// Every text-assistance feature is off, and it's marked secure so the
/// keyboard doesn't learn what's typed here (usually sign-in details).
final class KeyInputView: UIView, UIKeyInput {
    /// Called when the keyboard opens or closes.
    var onFocusChange: ((Bool) -> Void)?

    private let desktop: RemoteDesktop
    private lazy var keyBar = KeyBar(input: self)
    private(set) var control = false
    private(set) var alt = false

    init(desktop: RemoteDesktop) {
        self.desktop = desktop
        super.init(frame: .zero)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    // MARK: UIKeyInput

    override var canBecomeFirstResponder: Bool { true }
    var hasText: Bool { true }  // so Backspace always reaches us

    func insertText(_ text: String) {
        desktop.type(text, modifiers: takeModifiers())
    }

    func deleteBackward() {
        desktop.key(Keysym.backspace, modifiers: takeModifiers())
    }

    // MARK: UITextInputTraits

    var autocapitalizationType = UITextAutocapitalizationType.none
    var autocorrectionType = UITextAutocorrectionType.no
    var spellCheckingType = UITextSpellCheckingType.no
    var smartQuotesType = UITextSmartQuotesType.no
    var smartDashesType = UITextSmartDashesType.no
    var smartInsertDeleteType = UITextSmartInsertDeleteType.no
    var inlinePredictionType = UITextInlinePredictionType.no
    var keyboardType = UIKeyboardType.default
    var isSecureTextEntry = true

    // MARK: Focus

    override var inputAccessoryView: UIView? { keyBar }

    @discardableResult
    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        if became { onFocusChange?(true) }
        return became
    }

    @discardableResult
    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned { onFocusChange?(false) }
        return resigned
    }

    // MARK: Keys and modifiers

    /// Sends a key from the bar (Esc, Tab, arrows), with any sticky modifiers.
    func press(_ keysym: UInt32) {
        desktop.key(keysym, modifiers: takeModifiers())
    }

    func toggleControl() {
        control.toggle()
        keyBar.refresh()
    }

    func toggleAlt() {
        alt.toggle()
        keyBar.refresh()
    }

    /// Ctrl and Alt apply to the next key only.
    private func takeModifiers() -> [UInt32] {
        var modifiers: [UInt32] = []
        if control { modifiers.append(Keysym.control) }
        if alt { modifiers.append(Keysym.alt) }
        if !modifiers.isEmpty {
            control = false
            alt = false
            keyBar.refresh()
        }
        return modifiers
    }

    /// Hardware keyboards: keys UIKeyInput doesn't deliver.
    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var unhandled = Set<UIPress>()
        for keyPress in presses {
            let keysym: UInt32? =
                switch keyPress.key?.keyCode {
                case .keyboardLeftArrow: Keysym.left
                case .keyboardRightArrow: Keysym.right
                case .keyboardUpArrow: Keysym.up
                case .keyboardDownArrow: Keysym.down
                case .keyboardEscape: Keysym.escape
                case .keyboardHome: Keysym.home
                case .keyboardEnd: Keysym.end
                case .keyboardPageUp: Keysym.pageUp
                case .keyboardPageDown: Keysym.pageDown
                case .keyboardDeleteForward: Keysym.delete
                default: nil
                }
            if let keysym { press(keysym) } else { unhandled.insert(keyPress) }
        }
        if !unhandled.isEmpty { super.pressesBegan(unhandled, with: event) }
    }
}

/// The bar above the keyboard: Esc, Tab, sticky Ctrl and Alt, and arrows.
private final class KeyBar: UIInputView {
    private weak var input: KeyInputView?
    private var controlButton: UIButton?
    private var altButton: UIButton?

    init(input: KeyInputView) {
        self.input = input
        super.init(frame: CGRect(x: 0, y: 0, width: 0, height: 48), inputViewStyle: .keyboard)
        autoresizingMask = .flexibleWidth

        let control = button(title: "ctrl") { $0.toggleControl() }
        let alt = button(title: "alt") { $0.toggleAlt() }
        controlButton = control
        altButton = alt
        let stack = UIStackView(arrangedSubviews: [
            button(title: "esc") { $0.press(Keysym.escape) },
            button(title: "tab") { $0.press(Keysym.tab) },
            control,
            alt,
            button(symbol: "arrow.left") { $0.press(Keysym.left) },
            button(symbol: "arrow.up") { $0.press(Keysym.up) },
            button(symbol: "arrow.down") { $0.press(Keysym.down) },
            button(symbol: "arrow.right") { $0.press(Keysym.right) },
        ])
        stack.distribution = .fillEqually
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 6),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -6),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -4),
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    func refresh() {
        style(controlButton, on: input?.control == true)
        style(altButton, on: input?.alt == true)
    }

    private func style(_ button: UIButton?, on: Bool) {
        button?.configuration?.baseBackgroundColor = on ? .tintColor : .secondarySystemBackground
        button?.configuration?.baseForegroundColor = on ? .white : .label
    }

    private func button(title: String? = nil, symbol: String? = nil, action: @escaping (KeyInputView) -> Void)
        -> UIButton
    {
        var configuration = UIButton.Configuration.filled()
        configuration.baseBackgroundColor = .secondarySystemBackground
        configuration.baseForegroundColor = .label
        configuration.cornerStyle = .medium
        configuration.contentInsets = .zero
        if let title {
            var text = AttributedString(title)
            text.font = .systemFont(ofSize: 14, weight: .medium)
            configuration.attributedTitle = text
        }
        if let symbol { configuration.image = UIImage(systemName: symbol) }
        let button = UIButton(
            configuration: configuration,
            primaryAction: UIAction { [weak self] _ in
                if let input = self?.input { action(input) }
            })
        button.accessibilityLabel = title ?? symbol
        return button
    }
}
