package cuenv

#AwsSecret: #Resolver & {
	secretId: string
	ref:      "aws://\(secretId)"
	resolver: #ExecResolver & {
		command: "aws"
		args: [
			"secretsmanager", "get-secret-value",
			"--secret-id", secretId,
			"--query", "SecretString",
			"--output", "text",
		]
	}
}
