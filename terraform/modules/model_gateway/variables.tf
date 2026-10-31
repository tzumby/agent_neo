variable "name" {
  description = "Name prefix for resources."
  type        = string
}

variable "desktop_role_prefix" {
  description = "Desktop roles are this name or <name>-<user>; only their instance sessions may call the gateway."
  type        = string
}

variable "desktop_role_names" {
  description = "Desktop roles to grant permission to call the gateway's URL."
  type        = list(string)
}

variable "ssm_prefix" {
  description = "SSM prefix holding llm/together_api_key; the gateway's URL is written under it."
  type        = string
}

variable "prices" {
  description = "Allowed models and their USD price per 1M tokens: [input, cached input, output]."
  type        = map(list(number))
}

variable "daily_budget_usd" {
  description = "Model spend allowed per desktop per day (UTC)."
  type        = number
}

variable "source_dir" {
  description = "The gateway's code (services/model-gateway/src)."
  type        = string
}
