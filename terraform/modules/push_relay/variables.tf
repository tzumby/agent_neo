variable "name" {
  description = "Name prefix for resources."
  type        = string
}

variable "desktop_role_name" {
  description = "IAM role of the desktop instances. Only its instance sessions may call the relay."
  type        = string
}

variable "ssm_prefix" {
  description = "SSM prefix holding apns/key_p8 and apns/key_id (stored by make apns-key); the relay URL is written under it."
  type        = string
}

variable "apns_team_id" {
  description = "Apple Developer team ID (the JWT issuer)."
  type        = string
}

variable "apns_topic" {
  description = "The iOS app's bundle ID."
  type        = string
}

variable "apns_environment" {
  description = "sandbox (apps installed from Xcode) or production (TestFlight, App Store)."
  type        = string
  default     = "sandbox"

  validation {
    condition     = contains(["sandbox", "production"], var.apns_environment)
    error_message = "apns_environment must be sandbox or production."
  }
}

variable "source_dir" {
  description = "The relay's code (services/push-relay/src)."
  type        = string
}
