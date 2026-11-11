variable "region" {
  description = "AWS region. The Makefile sets it from AWS_REGION (TF_VAR_region)."
  type        = string
  default     = "ca-central-1"
}

variable "project" {
  description = "Project name, used in resource names, tags and SSM paths (/<project>/<env>/...)."
  type        = string
  default     = "cloud-desktops"
}

variable "env" {
  description = "This deployment's name. The Makefile sets it from ENV (TF_VAR_env). A second deployment in the same account needs its own."
  type        = string
  default     = "personal"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{1,19}$", var.env))
    error_message = "env is 2-20 lowercase letters, digits and dashes, starting with a letter."
  }
}

variable "vpc_cidr" {
  description = "CIDR block for the VPC (a /16). Nothing connects it to your other networks, so any /16 works."
  type        = string
  default     = "10.40.0.0/16"
}

variable "enable_nat_instance" {
  description = "Internet access for the private subnets through a fck-nat instance (a t4g.nano, about $3/month plus its public IPv4 address). Not highly available."
  type        = bool
  default     = true
}

variable "enable_nat_gateway" {
  description = "Internet access through AWS's NAT gateway instead (about $33/month): no third-party image, highly available."
  type        = bool
  default     = false
}

variable "enable_ssm_endpoints" {
  description = "Interface endpoints for SSM, so Session Manager doesn't go through the NAT (three endpoints in two AZs, about $45/month)."
  type        = bool
  default     = false
}

variable "desktop_arch" {
  description = "The desktop's CPU architecture: amd64 (x86_64: most Linux desktop apps ship only for it) or arm64 (Graviton, ~13% cheaper). desktop_instance_type must match, and so must DESKTOP_ARCH for the builds."
  type        = string
  default     = "amd64"

  validation {
    condition     = contains(["amd64", "arm64"], var.desktop_arch)
    error_message = "desktop_arch must be amd64 or arm64."
  }
}

variable "desktop_instance_type" {
  description = "Instance type for the desktop, of desktop_arch (amd64: e.g. t3a.medium; arm64: e.g. t4g.medium)."
  type        = string
  default     = "t3a.medium"
}

variable "cpu_credits" {
  description = "Burstable CPU credit mode: unlimited or standard."
  type        = string
  default     = "unlimited"

  validation {
    condition     = contains(["unlimited", "standard"], var.cpu_credits)
    error_message = "cpu_credits must be \"unlimited\" or \"standard\"."
  }
}

variable "disk_size_gb" {
  description = "The desktop's disk (gp3), in GB."
  type        = number
  default     = 20
}

variable "ubuntu_version" {
  description = "Ubuntu LTS release for the desktop's base image."
  type        = string
  default     = "26.04"
}

variable "tailnet_hostname" {
  description = "The desktop's device name on your tailnet: https://<this>.<your tailnet>.ts.net."
  type        = string
  default     = "neo-agent"

  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9-]{0,62}$", var.tailnet_hostname))
    error_message = "tailnet_hostname is lowercase letters, digits and dashes (a DNS label)."
  }
}
