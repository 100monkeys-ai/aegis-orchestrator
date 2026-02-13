#!/bin/bash

# Adapted from https://github.com/mrts/docker-postgresql-multiple-databases

set -e
set -u

function create_user_and_database() {
	local database=$1
	# Create Database (ignore error if exists)
	echo "  Creating database '$database' owned by '$POSTGRES_USER'"
	psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" -c "CREATE DATABASE $database OWNER $POSTGRES_USER;" || echo "Database $database creation skipped (may already exist)"
}

if [ -n "$POSTGRES_MULTIPLE_DATABASES" ]; then
	echo "Multiple database creation requested: $POSTGRES_MULTIPLE_DATABASES"
	for db in $(echo $POSTGRES_MULTIPLE_DATABASES | tr ',' ' '); do
		create_user_and_database $db
	done
	echo "Multiple databases created"
fi
