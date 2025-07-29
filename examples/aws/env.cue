package env

import "github.com/rawkode/cuenv"

// Environment configuration
env: cuenv.#Env & {
	TEST_VAR: "hello"
    ANOTHER: "world"
    MY_SECRET: cuenv.#AwsSecret & { secretId: "foo" }
}
