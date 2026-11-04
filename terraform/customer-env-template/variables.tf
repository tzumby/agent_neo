variable "account_id" {
  description = "The customer's sandbox account, the only one this env may touch."
  type        = string

  validation {
    condition     = can(regex("^[0-9]{12}$", var.account_id))
    error_message = "account_id is the 12-digit AWS account ID."
  }
}

variable "region" {
  description = "AWS region (the guardrails allow only this one)."
  type        = string
  default     = "ca-central-1"
}

variable "desktops" {
  description = "The agent desktops, by name."
  type = map(object({
    instance_type = optional(string)
  }))
  default = { agent = {} }
}

variable "desktop_instance_type" {
  type    = string
  default = "t3a.medium"
}

variable "enable_image_builder" {
  type    = bool
  default = false
}

variable "gateway_ami" {
  type    = string
  default = ""
}

variable "gateway_attested" {
  type    = bool
  default = false
}

variable "model_daily_budget_usd" {
  type    = number
  default = 20
}
