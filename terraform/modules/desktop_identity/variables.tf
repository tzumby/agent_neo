variable "role_name" {
  description = "The desktop's own role (<fleet role>-<user>), so policies can name exactly this desktop."
  type        = string
}

variable "user" {
  description = "Who the desktop belongs to (tag)."
  type        = string
}

variable "ssm_parameter_prefix" {
  description = "SSM path holding the project's secrets (e.g. /cloud-desktops); the desktop is denied reading it."
  type        = string
}

variable "permissions_boundary" {
  description = "Permissions boundary for the role (a customer deployment's: deploy/customer-aws). Null: none."
  type        = string
  default     = null
}

variable "own_parameters_path" {
  description = "SSM path only this desktop may read, e.g. /front-door/<env>/<name> (its tunnel token and Access settings). Outside ssm_parameter_prefix, which it may never read. Empty: none."
  type        = string
  default     = ""

  validation {
    condition     = var.own_parameters_path == "" || can(regex("^/[a-z0-9/-]+[a-z0-9]$", var.own_parameters_path))
    error_message = "own_parameters_path is an SSM path like /front-door/acme/agent, without a trailing slash."
  }
}
