output "state_bucket" {
  description = "Name of the Terraform state bucket (pass to `terraform init -backend-config`)."
  value       = aws_s3_bucket.state.bucket
}
