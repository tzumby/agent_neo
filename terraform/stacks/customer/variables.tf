variable "env" {
  description = "The customer's environment name (lowercase, e.g. acme): in resource names, the Env tag and the SSM paths /cloud-desktops/<env>/ and /front-door/<env>/."
  type        = string

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{1,19}$", var.env))
    error_message = "env is 2-20 lowercase letters, digits and dashes, starting with a letter."
  }
}

variable "permissions_boundary" {
  description = "ARN of the boundary every role here gets (deploy/customer-aws/boundary.json, created by the customer). Null only outside a customer account."
  type        = string
  default     = null
}

variable "vpc_cidr" {
  description = "The agent's VPC. Nothing connects it to the customer's other networks, so any /16 works."
  type        = string
  default     = "10.40.0.0/16"
}

variable "desktops" {
  description = "The agent desktops, by name (one for the first customer: agent). Each gets its own role, encryption key and front-door settings path."
  type = map(object({
    # Defaults to desktop_instance_type.
    instance_type = optional(string)
  }))
  default = { agent = {} }

  validation {
    condition     = alltrue([for name in keys(var.desktops) : can(regex("^[a-z][a-z0-9-]{1,19}$", name))])
    error_message = "Desktop names are 2-20 lowercase letters, digits and dashes, starting with a letter."
  }
}

variable "desktop_arch" {
  description = "Desktops' CPU architecture: amd64 or arm64. desktop_instance_type must match."
  type        = string
  default     = "amd64"

  validation {
    condition     = contains(["amd64", "arm64"], var.desktop_arch)
    error_message = "desktop_arch must be amd64 or arm64."
  }
}

variable "desktop_instance_type" {
  description = "Instance type for desktops (the guardrails' instance-type list must include it)."
  type        = string
  default     = "t3a.medium"
}

variable "cpu_credits" {
  description = "Burstable CPU credit mode: unlimited or standard."
  type        = string
  default     = "unlimited"
}

variable "ubuntu_version" {
  description = "Ubuntu LTS release for desktops."
  type        = string
  default     = "26.04"
}

variable "enable_image_builder" {
  description = "Role and security group for scripts/build-gateway-image.sh's temporary builder; on only while it runs."
  type        = bool
  default     = false
}

variable "gateway_ami" {
  description = "The model gateway's image, built in this account by scripts/build-gateway-image.sh. Empty: no gateway yet."
  type        = string
  default     = ""
}

variable "gateway_attested" {
  description = "Desktops check the gateway's NitroTPM attestation (a c7g.medium, about $29/month; the guardrails must allow it) instead of pinning the certificate it publishes to SSM (a t4g.micro, about $7/month)."
  type        = bool
  default     = false
}

variable "model_daily_budget_usd" {
  description = "Model spend allowed per desktop per day (UTC), enforced by the gateway, on the customer's Together key."
  type        = number
  default     = 20
}

variable "model_prices" {
  description = "Models allowed through the gateway, with Together's serverless prices: [input, cached input, output] USD per 1M tokens. Keep in step with envs/dev and agentd's llm.rs."
  type        = map(list(number))
  default = {
    "zai-org/GLM-5.3"                      = [1.40, 0.26, 4.40]
    "zai-org/GLM-5.3-Flash"                = [0.15, 0.03, 0.50]
    "moonshotai/Kimi-K3"                   = [3.00, 0.30, 15.00]
    "deepseek-ai/DeepSeek-V4-Pro-0813"     = [1.32, 0.13, 3.96]
    "deepseek-ai/DeepSeek-V4.1-Flash-1017" = [0.30, 0.006, 1.20]
  }
}
