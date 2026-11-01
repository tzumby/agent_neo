import AgentAPI
import SwiftUI

/// Tasks the agent runs on its own, set up by asking it ("every weekday at 8,
/// summarize my inbox"). Each can be paused or cancelled here.
struct SchedulesView: View {
    let client: AgentClient
    @State private var schedules: [AgentSchedule] = []
    @State private var loaded = false
    @State private var problem: String?

    var body: some View {
        List {
            if loaded, schedules.isEmpty {
                Section {
                    Text("Nothing scheduled. Ask the agent, like “Every weekday at 8, summarize my unread email.”")
                        .foregroundStyle(.secondary)
                }
            }
            if !schedules.isEmpty {
                Section {
                    ForEach(schedules) { schedule in
                        ScheduleRow(schedule: schedule)
                            .swipeActions(edge: .trailing) {
                                Button("Cancel", role: .destructive) { Task { await cancel(schedule) } }
                            }
                            .swipeActions(edge: .leading) {
                                Button(schedule.paused ? "Resume" : "Pause") {
                                    Task { await setPaused(!schedule.paused, schedule) }
                                }
                                .tint(schedule.paused ? .green : .orange)
                            }
                    }
                } footer: {
                    Text("Each run is a new task with just its prompt. Approvals still apply: you get a notification when one needs you. Swipe left to cancel, right to pause.")
                }
            }
            if let problem {
                Section { Text(problem).foregroundStyle(.red) }
            }
        }
        .navigationTitle("Schedules")
        .navigationBarTitleDisplayMode(.inline)
        .overlay { if !loaded { ProgressView() } }
        .task { await load() }
        .refreshable { await load() }
    }

    private func load() async {
        do {
            schedules = try await client.schedules()
            problem = nil
        } catch {
            problem = error.localizedDescription
        }
        loaded = true
    }

    private func cancel(_ schedule: AgentSchedule) async {
        schedules.removeAll { $0.id == schedule.id }
        do { try await client.cancel(schedule: schedule.id) } catch { await load() }
    }

    private func setPaused(_ paused: Bool, _ schedule: AgentSchedule) async {
        do {
            let updated = try await client.setPaused(paused, schedule: schedule.id)
            if let index = schedules.firstIndex(where: { $0.id == schedule.id }) { schedules[index] = updated }
        } catch {
            problem = error.localizedDescription
        }
    }
}

private struct ScheduleRow: View {
    let schedule: AgentSchedule

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text(schedule.name).font(.headline)
                if schedule.paused {
                    Text("Paused").font(.caption.weight(.semibold)).foregroundStyle(.orange)
                }
            }
            Text(schedule.description).font(.subheadline)
            if !schedule.paused {
                Text("Next: \(schedule.nextRun.formatted(.dateTime.weekday().month().day().hour().minute()))")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
            Text(schedule.prompt)
                .font(.footnote)
                .foregroundStyle(.secondary)
                .lineLimit(3)
            if schedule.timezone != TimeZone.current.identifier {
                Text("Times in \(schedule.timezone)").font(.caption).foregroundStyle(.tertiary)
            }
        }
        .padding(.vertical, 2)
    }
}
