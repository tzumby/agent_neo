variable "name" {
  description = "Name prefix for resources."
  type        = string
}

variable "vpc_id" {
  type = string
}

variable "app_port" {
  description = "Port the control-plane app listens on behind the ALB."
  type        = number
  default     = 8000
}

variable "desktop_agent_port" {
  description = "Port of the desktop agent daemon."
  type        = number
  default     = 8080
}

variable "novnc_port" {
  description = "Port of websockify/noVNC on desktops."
  type        = number
  default     = 6080
}
