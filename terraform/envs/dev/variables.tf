variable "region" {
  description = "AWS region."
  type        = string
  default     = "ca-central-1"
}

variable "project" {
  description = "Project name, used in resource names and tags."
  type        = string
  default     = "cloud-desktops"
}

variable "env" {
  description = "Environment name."
  type        = string
  default     = "dev"
}

variable "vpc_cidr" {
  description = "CIDR block for the VPC."
  type        = string
  default     = "10.40.0.0/16"
}

variable "enable_nat_gateway" {
  description = "Create a NAT gateway for private-subnet egress (~$1.10/day in ca-central-1)."
  type        = bool
  default     = false
}

variable "nat_per_az" {
  description = "One NAT gateway per AZ instead of one shared."
  type        = bool
  default     = false
}

variable "enable_ssm_endpoints" {
  description = "Interface endpoints for SSM so private instances need no NAT for Session Manager."
  type        = bool
  default     = false
}

variable "app_port" {
  description = "Control-plane app port behind the ALB."
  type        = number
  default     = 8000
}

variable "desktop_instance_type" {
  description = "Instance type for desktops (must be arm64/Graviton)."
  type        = string
  default     = "t4g.medium"
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

variable "ubuntu_version" {
  description = "Ubuntu LTS release for the desktop base image."
  type        = string
  default     = "26.04"
}

variable "create_test_desktop" {
  description = "Launch a single test desktop instance for developing the Ansible role."
  type        = bool
  default     = false
}

variable "test_desktop_public_ip" {
  description = "Temporary: put the test desktop in a public subnet with a public IP (no inbound rules) instead of behind a NAT gateway."
  type        = bool
  default     = false
}

variable "apns_team_id" {
  description = "Apple Developer team ID for push notifications."
  type        = string
  default     = "4UVAPZYV4Q"
}

variable "apns_topic" {
  description = "The iOS app's bundle ID."
  type        = string
  default     = "com.tzumby.clouddesktop"
}

variable "apns_environment" {
  description = "sandbox while the app is installed from Xcode; production for TestFlight or the App Store."
  type        = string
  default     = "sandbox"
}
