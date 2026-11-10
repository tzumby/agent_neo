terraform {
  # bucket and region are passed at init time from the bootstrap stack's
  # output: see `make init`.
  backend "s3" {
    key          = "terraform.tfstate"
    encrypt      = true
    use_lockfile = true
  }
}
