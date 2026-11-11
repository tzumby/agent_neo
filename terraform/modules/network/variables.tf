variable "name" {
  description = "Name prefix for resources."
  type        = string
}

variable "vpc_cidr" {
  description = "CIDR block for the VPC (a /16)."
  type        = string
}

variable "azs" {
  description = "Availability zones to use, one public and one private subnet each."
  type        = list(string)

  validation {
    condition     = length(var.azs) >= 2
    error_message = "Use at least two AZs."
  }
}

variable "enable_nat_gateway" {
  description = "Create NAT gateway(s) so private subnets can reach the internet."
  type        = bool
  default     = false
}

variable "nat_per_az" {
  description = "One NAT gateway per AZ (prod) instead of a single shared one (dev)."
  type        = bool
  default     = false
}

variable "enable_ssm_endpoints" {
  description = "Create interface endpoints for ssm, ssmmessages and ec2messages."
  type        = bool
  default     = false
}

variable "enable_nat_instance" {
  description = "Give private subnets internet access through a fck-nat instance (cheaper than NAT gateways, not highly available)."
  type        = bool
  default     = false
}

variable "nat_instance_type" {
  description = "Instance type for the fck-nat instance (arm64)."
  type        = string
  default     = "t4g.nano"
}
