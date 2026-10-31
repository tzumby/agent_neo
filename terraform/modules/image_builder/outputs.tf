output "instance_profile_name" {
  value = aws_iam_instance_profile.builder.name
}

output "security_group_id" {
  value = aws_security_group.builder.id
}
