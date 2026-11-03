variable "region" {
  description = "AWS region for the state bucket."
  type        = string
  default     = "ca-central-1"
}

variable "project" {
  description = "Project name, used in resource names and tags."
  type        = string
  default     = "cloud-desktops"
}

variable "account_id" {
  description = "For a customer's account (make bootstrap ENV=<customer>): the only account this may touch. Empty: ours (the default workspace)."
  type        = string
  default     = ""
}
