import Foundation

/// Spots an API key or token pasted into a chat message, so the app can offer
/// to save it on the desktop instead: anything sent in the chat goes to the
/// model and stays in the history.
public enum KeyDetector {
    public struct Found: Sendable, Equatable {
        /// The key itself.
        public let value: String
        /// A name for it, e.g. GITHUB_TOKEN.
        public let name: String
        /// What it is: "GitHub token".
        public let label: String
    }

    /// Well-known key formats, most specific first. Each has a prefix, so
    /// ordinary text (and hashes) doesn't match.
    private static let kinds: [(pattern: String, name: String, label: String)] = [
        (#"\bgithub_pat_[A-Za-z0-9_]{40,}"#, "GITHUB_TOKEN", "GitHub token"),
        (#"\bgh[pousr]_[A-Za-z0-9]{30,}"#, "GITHUB_TOKEN", "GitHub token"),
        (#"\bglpat-[A-Za-z0-9_\-]{20,}"#, "GITLAB_TOKEN", "GitLab token"),
        (#"\bsk-ant-[A-Za-z0-9_\-]{20,}"#, "ANTHROPIC_API_KEY", "Anthropic API key"),
        (#"\btgp_v1_[A-Za-z0-9_\-]{20,}"#, "TOGETHER_API_KEY", "Together AI API key"),
        (#"\b[sr]k_(live|test)_[A-Za-z0-9]{20,}"#, "STRIPE_API_KEY", "Stripe API key"),
        (#"\bsk-[A-Za-z0-9_\-]{20,}"#, "OPENAI_API_KEY", "OpenAI API key"),
        (#"\bxox[abposr]-[A-Za-z0-9\-]{10,}"#, "SLACK_TOKEN", "Slack token"),
        (#"\bhf_[A-Za-z0-9]{30,}"#, "HF_TOKEN", "Hugging Face token"),
        (#"\bAIza[0-9A-Za-z_\-]{35}"#, "GOOGLE_API_KEY", "Google API key"),
        (#"\bAKIA[0-9A-Z]{16}\b"#, "AWS_ACCESS_KEY_ID", "AWS access key ID"),
    ]

    public static func find(in text: String) -> Found? {
        for kind in kinds {
            guard let range = text.range(of: kind.pattern, options: .regularExpression) else { continue }
            return Found(value: String(text[range]), name: kind.name, label: kind.label)
        }
        return nil
    }

    /// The name a key would be saved as: capitals, digits and underscores.
    public static func normalizedName(_ text: String) -> String {
        let upper = text.uppercased().map { $0 == " " || $0 == "-" ? "_" : $0 }
        return String(upper.filter { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "_") }.prefix(64))
    }

    /// Whether agentd accepts this name (secrets.rs: check_name).
    public static func isValidName(_ name: String) -> Bool {
        name.range(of: #"^[A-Z][A-Z0-9_]{1,63}$"#, options: .regularExpression) != nil
            && !["PASSWORD", "PASSWD", "PASSCODE"].contains { name.contains($0) }
    }
}
