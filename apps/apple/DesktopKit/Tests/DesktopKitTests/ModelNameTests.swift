import Testing

@testable import AgentAPI

@Test func modelsHaveReadableNames() {
    #expect(ModelName.display("zai-org/GLM-5.3") == "GLM 5.3")
    #expect(ModelName.display("zai-org/GLM-5.3-Flash") == "GLM 5.3 Flash")
    #expect(ModelName.display("moonshotai/Kimi-K3") == "Kimi K3")
    #expect(ModelName.display("deepseek-ai/DeepSeek-V4-Pro-0813") == "DeepSeek V4 Pro")
    #expect(ModelName.display("deepseek-ai/DeepSeek-V4.1-Flash-1017") == "DeepSeek V4.1 Flash")
    #expect(ModelName.display("some-model") == "some model")
}
