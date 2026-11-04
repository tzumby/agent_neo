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

variable "desktop_arch" {
  description = "Desktops' CPU architecture: amd64 (x86_64: most Linux desktop apps, e.g. Slack, ship only for it) or arm64 (Graviton). desktop_instance_type must match."
  type        = string
  default     = "amd64"

  validation {
    condition     = contains(["amd64", "arm64"], var.desktop_arch)
    error_message = "desktop_arch must be amd64 or arm64."
  }
}

variable "desktop_instance_type" {
  description = "Instance type for desktops, of desktop_arch (amd64: e.g. t3a.medium; arm64: e.g. t4g.medium)."
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

variable "enable_nat_instance" {
  description = "Internet access for private subnets through a fck-nat instance (instead of NAT gateways)."
  type        = bool
  default     = false
}

variable "users" {
  description = "Users with a desktop; each gets their own encryption key."
  type        = list(string)
  default     = ["tzumby"]
}

variable "desktops" {
  description = "Desktops besides the test desktop, one per user, by name (a tester, later users). Each joins the desktops tailnet, where only its user's devices reach it. Each name must also be in users (its encryption key). Added with scripts/new-desktop.sh."
  type = map(object({
    # Defaults to desktop_instance_type.
    instance_type = optional(string)
  }))
  default = {}

  validation {
    condition     = alltrue([for name in keys(var.desktops) : can(regex("^[a-z][a-z0-9-]{1,19}$", name))])
    error_message = "Desktop names are 2-20 lowercase letters, digits and dashes, starting with a letter."
  }

  validation {
    condition     = alltrue([for name in keys(var.desktops) : contains(var.users, name)])
    error_message = "Every desktop's name must also be in users, which gives it its own encryption key."
  }

  validation {
    condition     = !contains(keys(var.desktops), var.test_desktop_user)
    error_message = "The test desktop's user already has a desktop (the test desktop)."
  }
}

variable "test_desktop_user" {
  description = "Whose desktop the test desktop is (picks its role and encryption key)."
  type        = string
  default     = "tzumby"
}

variable "model_daily_budget_usd" {
  description = "Model spend allowed per desktop per day (UTC), enforced by the model gateway."
  type        = number
  default     = 20
}

variable "enable_image_builder" {
  description = "IAM role and security group for scripts/build-gateway-image.sh's temporary builder."
  type        = bool
  default     = false
}

variable "gateway_ami" {
  description = "Model gateway image from scripts/build-gateway-image.sh. Empty: no gateway yet."
  type        = string
  default     = ""
}

variable "gateway_attested" {
  description = "Desktops check the gateway's NitroTPM attestation (a c7g.medium, about $29/month) instead of pinning the certificate it publishes to SSM (a t4g.micro, about $7/month)."
  type        = bool
  default     = false
}
