import Foundation

/// How the apps name a model: "zai-org/GLM-5.3-Flash" is "GLM 5.3 Flash",
/// "deepseek-ai/DeepSeek-V4.1-Flash-1017" is "DeepSeek V4.1 Flash" (the
/// provider and a snapshot's date dropped).
public enum ModelName {
    public static func display(_ id: String) -> String {
        var name = id.split(separator: "/").last.map(String.init) ?? id
        if let range = name.range(of: #"-\d{4}$"#, options: .regularExpression) {
            name.removeSubrange(range)
        }
        return name.replacingOccurrences(of: "-", with: " ")
    }
}
