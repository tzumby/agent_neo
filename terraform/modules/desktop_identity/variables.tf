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
