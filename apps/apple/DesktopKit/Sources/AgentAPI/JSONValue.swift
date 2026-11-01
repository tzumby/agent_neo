import Foundation

/// Any JSON value. agentd's event payloads differ per kind; ChatCore reads the
/// fields it needs through the accessors.
public enum JSONValue: Decodable, Sendable, Equatable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([JSONValue].self) {
            self = .array(value)
        } else {
            self = .object(try container.decode([String: JSONValue].self))
        }
    }

    public subscript(key: String) -> JSONValue? {
        if case .object(let object) = self { object[key] } else { nil }
    }

    public var string: String? {
        if case .string(let value) = self { value } else { nil }
    }

    public var double: Double? {
        if case .number(let value) = self { value } else { nil }
    }

    public var int: Int? { double.map { Int($0) } }

    public var bool: Bool? {
        if case .bool(let value) = self { value } else { nil }
    }

    public var array: [JSONValue]? {
        if case .array(let value) = self { value } else { nil }
    }
}
