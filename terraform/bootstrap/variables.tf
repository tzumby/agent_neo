variable "region" {
  description = "AWS region for the state bucket. The Makefile sets it from AWS_REGION (TF_VAR_region)."
  type        = string
  default     = "ca-central-1"
}

variable "project" {
  description = "Project name, used in resource names and tags."
  type        = string
  default     = "cloud-desktops"
}

variable "env" {
  description = "The deployment this bucket holds the state of. The Makefile sets it from ENV (TF_VAR_env)."
  type        = string
  default     = "personal"
}
