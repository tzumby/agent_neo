import Testing

@testable import ChatCore

@Test func spotsKeysPastedIntoMessages() throws {
    let github = try #require(KeyDetector.find(in: "clone acme/api with ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab please"))
    #expect(github == KeyDetector.Found(value: "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ab", name: "GITHUB_TOKEN", label: "GitHub token"))
    let pat = "github_pat_11ABCDEFG0abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJ"
    #expect(KeyDetector.find(in: pat)?.value == pat)
    #expect(KeyDetector.find(in: "key: sk-ant-api03-abcdefghijklmnopqrstuvwxyz")?.name == "ANTHROPIC_API_KEY")
    #expect(KeyDetector.find(in: "sk-proj-abcdefghijklmnopqrstuvwxyz012345")?.name == "OPENAI_API_KEY")
    #expect(KeyDetector.find(in: "AKIAIOSFODNN7EXAMPLE")?.name == "AWS_ACCESS_KEY_ID")
    // Ordinary text, hashes and words that only start the same way don't count.
    for text in [
        "Summarize my inbox", "commit 3f2a9c1e8b7d6a5f4e3d2c1b0a9f8e7d6c5b4a39", "ask the sk-team about it",
        "ghp_short",
    ] {
        #expect(KeyDetector.find(in: text) == nil, "\(text)")
    }
}

@Test func keyNamesFollowAgentdsRules() {
    #expect(KeyDetector.normalizedName("together api-key") == "TOGETHER_API_KEY")
    #expect(KeyDetector.isValidName("GITHUB_TOKEN"))
    #expect(!KeyDetector.isValidName("1TOKEN"))
    #expect(!KeyDetector.isValidName("G"))
    #expect(!KeyDetector.isValidName("GMAIL_PASSWORD"))
}
