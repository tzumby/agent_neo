variable "name" {
  description = "Name prefix for resources."
  type        = string
}

variable "ssm_transfer_bucket_arn" {
  description = "ARN of the bucket the Ansible aws_ssm connection uses for file transfer."
  type        = string
}
