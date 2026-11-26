import AgentAPI
import Charts
import ChatCore
import SwiftUI

/// What the model calls cost (Settings → Usage): this month against the
/// budget, today, last month and all time; the last 30 days as a bar chart;
/// this month by kind, model, conversation and schedule; and the monthly
/// budget, set or removed here. Estimates from token counts.
struct UsageView: View {
    let store: ChatStore
    let client: AgentClient
    @State private var usage: AgentUsage?
    @State private var problem: String?

    var body: some View {
        GroupedList {
            if let usage {
                Section {
                    Summary(usage: usage)
                }
                Section {
                    DailyChart(usage: usage)
                } header: {
                    Text("Last 30 days")
                }
                breakdowns(usage)
                BudgetSection(store: store, client: client, budget: usage.budget) { await load() }
            }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle("Usage")
        .inlineTitle()
        .overlay { if usage == nil, problem == nil { ProgressView() } }
        .task { await load() }
        .refreshable { await load() }
    }

    @ViewBuilder private func breakdowns(_ usage: AgentUsage) -> some View {
        let total = max(usage.thisMonth, usage.byKind.map(\.cost).reduce(0, +))
        if !usage.byKind.isEmpty {
            Section("This month by kind") {
                ForEach(usage.byKind) { kind in
                    CostRow(title: Self.name(of: kind.kind), detail: nil, cost: kind.cost, total: total)
                }
            }
        }
        if !usage.byModel.isEmpty {
            Section("By model") {
                ForEach(usage.byModel) { model in
                    CostRow(
                        title: ModelName.display(model.model),
                        detail: [Self.runs(model.runs), Self.tokens(model)].compactMap { $0 }.joined(separator: " · "),
                        cost: model.cost, total: total)
                }
            }
        }
        if !usage.bySession.isEmpty {
            Section("By conversation") {
                ForEach(usage.bySession) { session in
                    CostRow(
                        title: session.title.flatMap { $0.isEmpty ? nil : $0 }
                            ?? (session.id == AgentSession.main ? "First conversation" : "New conversation"),
                        detail: Self.runs(session.runs), cost: session.cost, total: total)
                }
            }
        }
        if !usage.bySchedule.isEmpty {
            Section {
                ForEach(usage.bySchedule) { schedule in
                    CostRow(
                        title: schedule.name ?? "Deleted schedule", detail: Self.runs(schedule.runs),
                        cost: schedule.cost, total: total)
                }
            } header: {
                Text("By schedule")
            } footer: {
                Text(estimateNote(usage)).sectionFooter()
            }
        } else {
            Section {
            } footer: {
                Text(estimateNote(usage)).sectionFooter()
            }
        }
    }

    private func estimateNote(_ usage: AgentUsage) -> String {
        var note = "Estimates in US dollars, from the model calls' token counts and your desktop's price list: not a bill."
        if let zone = usage.timezone { note += " Days and months are counted in \(zone)." }
        return note
    }

    private func load() async {
        do {
            usage = try await client.usage(days: 30)
            problem = nil
        } catch {
            problem = "Couldn't load usage. \(ChatStore.message(error))"
        }
    }

    static func name(of kind: String) -> String {
        switch kind {
        case "chat": "Chat"
        case "scheduled": "Scheduled tasks"
        case "tidy_up": "Memory tidy-up"
        default: kind.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }

    static func runs(_ count: Int) -> String? {
        count == 0 ? nil : count == 1 ? "1 run" : "\(count.formatted()) runs"
    }

    /// "1.2M tokens", counting what was read, written and cached.
    static func tokens(_ model: AgentUsage.ModelCost) -> String? {
        let total = model.inputTokens + model.outputTokens
        guard total > 0 else { return nil }
        return "\(total.formatted(.number.notation(.compactName))) tokens"
    }
}

/// Dollars: "$41.20", "< $0.01" for a fraction of a cent. The narrow
/// symbol: agentd's figures are always US dollars, and "US$" on every one
/// (as a Canadian locale writes it) is noise; the screen says so once.
enum Money {
    static func text(_ amount: Double, code: String = "USD") -> String {
        if amount > 0, amount < 0.01 {
            return "< \(0.01.formatted(style(code)))"
        }
        return amount.formatted(style(code).precision(.fractionLength(2)))
    }

    static func style(_ code: String) -> FloatingPointFormatStyle<Double>.Currency {
        .currency(code: code).presentation(.narrow)
    }
}

// MARK: - The headline

/// This month in large type with the budget's meter, then today, last month
/// and all time.
private struct Summary: View {
    let usage: AgentUsage

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 4) {
                Text("This month")
                    .font(.app(.subheadline))
                    .foregroundStyle(.secondary)
                Text(Money.text(usage.thisMonth, code: usage.currency))
                    .font(.system(size: 40, weight: .semibold, design: .rounded))
                    .monospacedDigit()
                    .contentTransition(.numericText())
                if let budget = usage.budget, let limit = budget.monthlyUsd {
                    BudgetMeter(budget: budget, limit: limit, currency: usage.currency)
                        .padding(.top, 6)
                }
            }
            Divider().overlay(Theme.hairline)
            HStack(alignment: .top, spacing: 12) {
                Figure(label: "Today", value: Money.text(usage.today, code: usage.currency))
                Figure(label: "Last month", value: Money.text(usage.lastMonth, code: usage.currency))
                Figure(label: "All time", value: Money.text(usage.allTime, code: usage.currency))
            }
        }
        .padding(.vertical, 8)
    }
}

private struct Figure: View {
    let label: String
    let value: String

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label)
                .font(.app(.caption))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.app(.headline))
                .monospacedDigit()
                .lineLimit(1)
                .minimumScaleFactor(0.8)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
    }
}

/// How much of the budget is used: a bar whose fill turns orange from 80%
/// and red once it's used up, always with the words beside it.
private struct BudgetMeter: View {
    let budget: Budget
    let limit: Double
    let currency: String

    private var fraction: Double { budget.fraction ?? (limit > 0 ? budget.spent / limit : 0) }

    private var tint: Color {
        if budget.paused || fraction >= 1 { return .red }
        return fraction >= 0.8 ? .orange : .green
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(tint.opacity(0.18))
                    Capsule()
                        .fill(tint)
                        .frame(width: max(6, geometry.size.width * min(fraction, 1)))
                }
            }
            .frame(height: 6)
            HStack(spacing: 6) {
                if budget.paused {
                    Image(systemName: "pause.circle.fill").foregroundStyle(tint)
                    Text("Used up: new messages wait")
                } else {
                    Text("\(Int((fraction * 100).rounded()))% of your \(Money.text(limit, code: currency)) budget")
                }
                Spacer()
                if !budget.paused, limit > budget.spent {
                    Text("\(Money.text(limit - budget.spent, code: currency)) left")
                        .foregroundStyle(.secondary)
                }
            }
            .font(.app(.footnote))
            .monospacedDigit()
        }
        .accessibilityElement(children: .combine)
    }
}

// MARK: - The chart

/// One bar a day, oldest first. Touching (or hovering over) a day shows its
/// cost and runs; otherwise the period's total sits above the bars.
private struct DailyChart: View {
    let usage: AgentUsage
    @State private var selected: Date?

    private struct Bar: Identifiable {
        let id: String
        let day: Date
        let cost: Double
        let runs: Int
    }

    private var bars: [Bar] {
        usage.daily.compactMap { entry in
            entry.day.map { Bar(id: entry.date, day: $0, cost: entry.cost, runs: entry.runs) }
        }
    }

    private var picked: Bar? {
        guard let selected else { return nil }
        return bars.first { Calendar.current.isDate($0.day, inSameDayAs: selected) }
    }

    var body: some View {
        let bars = bars
        let total = bars.map(\.cost).reduce(0, +)
        VStack(alignment: .leading, spacing: 12) {
            caption(total: total)
            Chart(bars) { bar in
                BarMark(
                    x: .value("Day", bar.day, unit: .day),
                    y: .value("Cost", bar.cost),
                    width: .ratio(0.62)
                )
                .cornerRadius(2.5)
                .foregroundStyle(Theme.prominent.opacity(picked == nil || picked?.id == bar.id ? 0.85 : 0.25))
            }
            .chartXSelection(value: $selected)
            .chartYAxis {
                AxisMarks(position: .trailing, values: .automatic(desiredCount: 3)) { value in
                    AxisGridLine(stroke: StrokeStyle(lineWidth: 0.5))
                        .foregroundStyle(Theme.hairline)
                    AxisValueLabel {
                        if let amount = value.as(Double.self) {
                            Text(amount.formatted(Money.style(usage.currency).precision(.fractionLength(0...2))))
                        }
                    }
                    .foregroundStyle(Color.secondary)
                }
            }
            .chartXAxis {
                AxisMarks(values: .stride(by: .day, count: 7)) { _ in
                    AxisValueLabel(format: .dateTime.month(.abbreviated).day(), centered: true)
                        .foregroundStyle(Color.secondary)
                }
            }
            .frame(height: 170)
            .overlay {
                if total == 0 {
                    Text("Nothing spent in these 30 days")
                        .font(.app(.footnote))
                        .foregroundStyle(.secondary)
                }
            }
            .accessibilityLabel("Cost per day, last 30 days")
        }
        .padding(.vertical, 8)
    }

    /// The day touched, or the whole period.
    private func caption(total: Double) -> some View {
        HStack(alignment: .firstTextBaseline) {
            if let picked {
                Text(picked.day.formatted(.dateTime.weekday(.abbreviated).month(.abbreviated).day()))
                    .foregroundStyle(.secondary)
                Spacer()
                Text(UsageView.runs(picked.runs) ?? "No runs")
                    .foregroundStyle(.secondary)
                Text(Money.text(picked.cost, code: usage.currency))
                    .fontWeight(.semibold)
            } else {
                Text("Total")
                    .foregroundStyle(.secondary)
                Spacer()
                Text(Money.text(total, code: usage.currency))
                    .fontWeight(.semibold)
            }
        }
        .font(.app(.subheadline))
        .monospacedDigit()
    }
}

// MARK: - Breakdowns

/// A line of a breakdown: what, how many runs, its cost, and a thin bar for
/// its share of the month.
private struct CostRow: View {
    let title: String
    let detail: String?
    let cost: Double
    let total: Double

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(title).lineLimit(1)
                    if let detail, !detail.isEmpty {
                        Text(detail)
                            .font(.app(.caption))
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer()
                Text(Money.text(cost))
                    .monospacedDigit()
            }
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(Theme.chip)
                    Capsule()
                        .fill(Theme.prominent.opacity(0.75))
                        .frame(width: max(3, geometry.size.width * share))
                }
            }
            .frame(height: 3)
            .accessibilityHidden(true)
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
    }

    private var share: Double { total > 0 ? min(cost / total, 1) : 0 }
}

// MARK: - Budget

#if DEBUG
    /// Debug builds only (ChatPreview's PREVIEW_SCREEN=usage-budget): the
    /// budget section alone, to see its row at a sheet's width.
    struct UsageBudgetPreview: View {
        let store: ChatStore
        let client: AgentClient
        @State private var usage: AgentUsage?

        var body: some View {
            GroupedList {
                if let usage {
                    BudgetSection(store: store, client: client, budget: usage.budget) { await load() }
                }
            }
            .task { await load() }
        }

        private func load() async { usage = try? await client.usage(days: 30) }
    }
#endif

/// The monthly budget: a field to set it, and a way to remove it.
private struct BudgetSection: View {
    let store: ChatStore
    let client: AgentClient
    let budget: Budget?
    let reload: () async -> Void
    @State private var text = ""
    @State private var saving = false
    @State private var problem: String?
    @FocusState private var editing: Bool

    private var current: Double? { budget?.monthlyUsd }

    /// What's typed, as dollars (nil: not a number).
    private var typed: Double? {
        let cleaned = text.replacingOccurrences(of: "$", with: "").trimmingCharacters(in: .whitespaces)
        return try? Double(cleaned, format: .number)
    }

    private var valid: Bool { typed.map { $0 > 0 && $0 <= 100_000 } ?? false }

    var body: some View {
        Section {
            HStack {
                Text("Monthly budget")
                Spacer()
                // As wide as what's typed (the placeholder only when empty),
                // so the dollar sign stays against the number. "None" is a
                // prompt and the title is hidden: the Mac's grouped form
                // shows a text field's title as a label of its own, at the
                // row's leading edge, on top of "Monthly budget".
                HStack(spacing: 1) {
                    if !text.isEmpty { Text("$").foregroundStyle(.secondary) }
                    TextField("Monthly budget", text: $text, prompt: text.isEmpty ? Text("None") : nil)
                        .labelsHidden()
                        .multilineTextAlignment(.trailing)
                        .monospacedDigit()
                        .focused($editing)
                        .onSubmit { if valid, typed != current { save(typed) } }
                        #if os(iOS)
                            .keyboardType(.decimalPad)
                        #else
                            .textFieldStyle(.plain)
                        #endif
                        .fixedSize()
                }
                .frame(minWidth: 80, alignment: .trailing)
                .contentShape(.rect)
                .onTapGesture { editing = true }
            }
            if valid, typed != current {
                Button {
                    save(typed)
                } label: {
                    HStack {
                        Text(current == nil ? "Set the budget" : "Save the budget")
                        if saving {
                            Spacer()
                            ProgressView().controlSize(.small)
                        }
                    }
                }
                .disabled(saving)
            }
            if current != nil {
                Button("Remove the budget", role: .destructive) { save(nil) }
                    .disabled(saving)
            }
            if let problem {
                Text(problem).font(.app(.footnote)).foregroundStyle(.red)
            }
        } header: {
            Text("Budget")
        } footer: {
            Text("You get a notification at 80% and at 100%. Once it's used up, new messages wait until you raise it or the month turns over; tasks already running finish.").sectionFooter()
        }
        .onAppear { show(current) }
        .onChange(of: current) { _, value in if !editing { show(value) } }
    }

    private func show(_ value: Double?) {
        text = value.map { $0.formatted(.number.precision(.fractionLength(0...2)).grouping(.never)) } ?? ""
    }

    private func save(_ value: Double?) {
        saving = true
        editing = false
        Task {
            problem = await store.setMonthlyBudget(value, client: client)
            await reload()
            if problem == nil { show(value) }
            saving = false
        }
    }
}
