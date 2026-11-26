import Foundation

/// How the apps name a model: "zai-org/GLM-5.3-Flash" is "GLM 5.3 Flash",
/// "deepseek-ai/DeepSeek-V4.1-Flash-1017" is "DeepSeek V4.1 Flash" (the
/// provider and a snapshot's date dropped), "claude-opus-4-6" is "Claude Opus
/// 4.6".
public enum ModelName {
    public static func display(_ id: String) -> String {
        // Claude's: family, then the version's dot.
        if let match = id.wholeMatch(of: #/claude-([a-z]+)-(\d+)(?:-(\d+))?/#) {
            let version = match.3.map { "\(match.2).\($0)" } ?? String(match.2)
            return "Claude \(match.1.prefix(1).uppercased())\(match.1.dropFirst()) \(version)"
        }
        var name = id.split(separator: "/").last.map(String.init) ?? id
        if let range = name.range(of: #"-\d{4}$"#, options: .regularExpression) {
            name.removeSubrange(range)
        }
        return name.replacingOccurrences(of: "-", with: " ")
    }
}
