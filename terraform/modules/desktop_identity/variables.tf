variable "role_name" {
  description = "The desktop's role and instance profile."
  type        = string
}

variable "ssm_parameter_prefix" {
  description = "SSM path holding the project's secrets (e.g. /neo-agent); the desktop is denied reading it."
  type        = string
}
