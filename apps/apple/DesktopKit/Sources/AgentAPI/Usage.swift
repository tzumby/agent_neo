import Foundation

/// What the model calls cost (GET /api/v1/usage, agentd 0.16.0): estimates
/// from token counts and agentd's price table, in the user's time zone.
public struct AgentUsage: Decodable, Sendable, Equatable {
    public let currency: String
    /// Worked out from token counts, not a bill.
    public let estimated: Bool
    /// Where days and months are counted, e.g. America/Toronto.
    public let timezone: String?
    /// "2026-11".
    public let month: String?
    public let today: Double
    public let thisMonth: Double
    public let lastMonth: Double
    public let allTime: Double
    /// One entry per day of the period asked for, oldest first, zeros included.
    public let daily: [Day]
    /// This month by kind (chat, scheduled, tidy_up), most expensive first.
    public let byKind: [Kind]
    public let byModel: [ModelCost]
    /// At most 10; a deleted conversation keeps its title.
    public let bySession: [SessionCost]
    /// At most 10; a deleted schedule has no name.
    public let bySchedule: [ScheduleCost]
    public let budget: Budget?

    public struct Day: Decodable, Sendable, Equatable, Identifiable {
        /// "2026-10-26", in the user's time zone.
        public let date: String
        public let cost: Double
        public let runs: Int

        public var id: String { date }

        public init(date: String, cost: Double, runs: Int) {
            self.date = date
            self.cost = cost
            self.runs = runs
        }

        enum CodingKeys: String, CodingKey { case date, cost, runs }

        public init(from decoder: any Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            date = try c.decode(String.self, forKey: .date)
            cost = try c.decodeIfPresent(Double.self, forKey: .cost) ?? 0
            runs = try c.decodeIfPresent(Int.self, forKey: .runs) ?? 0
        }

        /// Noon of that day here, so a chart's day doesn't slip across time zones.
        public var day: Date? {
            let parts = date.split(separator: "-").compactMap { Int($0) }
            guard parts.count == 3 else { return nil }
            return Calendar.current.date(from: DateComponents(year: parts[0], month: parts[1], day: parts[2], hour: 12))
        }
    }

    public struct Kind: Sendable, Equatable, Identifiable {
        /// chat, scheduled or tidy_up.
        public let kind: String
        public let cost: Double

        public var id: String { kind }

        public init(kind: String, cost: Double) {
            self.kind = kind
            self.cost = cost
        }
    }

    public struct ModelCost: Decodable, Sendable, Equatable, Identifiable {
        public let model: String
        public let cost: Double
        public let runs: Int
        /// Runs from before 0.16.0 count as 0.
        public let inputTokens: Int
        public let outputTokens: Int
        public let cachedTokens: Int

        public var id: String { model }

        enum CodingKeys: String, CodingKey {
            case model, cost, runs
            case inputTokens = "input_tokens"
            case outputTokens = "output_tokens"
            case cachedTokens = "cached_tokens"
        }

        public init(from decoder: any Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            model = try c.decodeIfPresent(String.self, forKey: .model) ?? ""
            cost = try c.decodeIfPresent(Double.self, forKey: .cost) ?? 0
            runs = try c.decodeIfPresent(Int.self, forKey: .runs) ?? 0
            inputTokens = try c.decodeIfPresent(Int.self, forKey: .inputTokens) ?? 0
            outputTokens = try c.decodeIfPresent(Int.self, forKey: .outputTokens) ?? 0
            cachedTokens = try c.decodeIfPresent(Int.self, forKey: .cachedTokens) ?? 0
        }
    }

    public struct SessionCost: Decodable, Sendable, Equatable, Identifiable {
        public let id: String
        public let title: String?
        public let cost: Double
        public let runs: Int

        enum CodingKeys: String, CodingKey { case id, title, cost, runs }

        public init(from decoder: any Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            id = try c.decode(String.self, forKey: .id)
            title = try c.decodeIfPresent(String.self, forKey: .title)
            cost = try c.decodeIfPresent(Double.self, forKey: .cost) ?? 0
            runs = try c.decodeIfPresent(Int.self, forKey: .runs) ?? 0
        }
    }

    public struct ScheduleCost: Decodable, Sendable, Equatable, Identifiable {
        public let id: Int
        /// nil: the schedule was deleted.
        public let name: String?
        public let cost: Double
        public let runs: Int

        enum CodingKeys: String, CodingKey { case id, name, cost, runs }

        public init(from decoder: any Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            id = try c.decodeIfPresent(Int.self, forKey: .id) ?? 0
            name = try c.decodeIfPresent(String.self, forKey: .name)
            cost = try c.decodeIfPresent(Double.self, forKey: .cost) ?? 0
            runs = try c.decodeIfPresent(Int.self, forKey: .runs) ?? 0
        }
    }

    enum CodingKeys: String, CodingKey {
        case currency, estimated, timezone, month, today, daily, budget
        case thisMonth = "this_month"
        case lastMonth = "last_month"
        case allTime = "all_time"
        case byKind = "by_kind"
        case byModel = "by_model"
        case bySession = "by_session"
        case bySchedule = "by_schedule"
    }

    public init(from decoder: any Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        currency = try c.decodeIfPresent(String.self, forKey: .currency) ?? "USD"
        estimated = try c.decodeIfPresent(Bool.self, forKey: .estimated) ?? true
        timezone = try c.decodeIfPresent(String.self, forKey: .timezone)
        month = try c.decodeIfPresent(String.self, forKey: .month)
        today = try c.decodeIfPresent(Double.self, forKey: .today) ?? 0
        thisMonth = try c.decodeIfPresent(Double.self, forKey: .thisMonth) ?? 0
        lastMonth = try c.decodeIfPresent(Double.self, forKey: .lastMonth) ?? 0
        allTime = try c.decodeIfPresent(Double.self, forKey: .allTime) ?? 0
        daily = try c.decodeIfPresent([Day].self, forKey: .daily) ?? []
        // An object of kind → cost; kinds this app doesn't know yet still count.
        let kinds = try c.decodeIfPresent([String: JSONValue].self, forKey: .byKind) ?? [:]
        byKind = kinds.compactMap { kind, cost in cost.double.map { Kind(kind: kind, cost: $0) } }
            .sorted { ($0.cost, $1.kind) > ($1.cost, $0.kind) }
        byModel = try c.decodeIfPresent([ModelCost].self, forKey: .byModel) ?? []
        bySession = try c.decodeIfPresent([SessionCost].self, forKey: .bySession) ?? []
        bySchedule = try c.decodeIfPresent([ScheduleCost].self, forKey: .bySchedule) ?? []
        budget = try c.decodeIfPresent(Budget.self, forKey: .budget)
    }
}

extension AgentClient {
    /// What the last `days` days (1–90) cost, with this month's breakdowns and
    /// the budget. agentd before 0.16.0 answers 404.
    public func usage(days: Int = 30) async throws -> AgentUsage {
        let url = baseURL.appending(path: "/api/v1/usage")
            .appending(queryItems: [URLQueryItem(name: "days", value: String(days))])
        let (data, response) = try await session.data(from: url)
        try Self.check(response, data)
        return try JSONDecoder().decode(AgentUsage.self, from: data)
    }

    /// Sets the monthly budget in US dollars, or removes it (nil). 400 unless
    /// it's a positive number up to 100,000.
    public func setMonthlyBudget(_ usd: Double?) async throws {
        try await perform("PUT", "/api/v1/settings", body: BudgetSetting(usd: usd))
    }
}

/// `{"monthly_budget_usd": 50}`, or an explicit null to remove it.
struct BudgetSetting: Encodable {
    let usd: Double?

    enum CodingKeys: String, CodingKey { case usd = "monthly_budget_usd" }

    func encode(to encoder: any Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(usd, forKey: .usd)
    }
}
