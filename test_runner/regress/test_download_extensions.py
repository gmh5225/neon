import json
import os
from contextlib import closing
from io import BytesIO

import pytest
from fixtures.log_helper import log
from fixtures.neon_fixtures import (
    NeonEnvBuilder,
    RemoteStorageKind,
)

"""
TODO:
status:
it appears that list_files on a non-existing path is bad whether you are real or mock s3 storage 

1. debug real s3 tests: I think the paths were slightly different than I was expecting
2. Make sure it gracefully is sad when tenant is not found
stderr: command failed: unexpected compute status: Empty

3. clean up the junk I put in the bucket (one time task)
4. can we simultaneously do MOCK and REAL s3 tests, or are the env vars conflicting/
5. libs/remote_storage/src/s3_bucket.rs TODO // TODO: if bucket prefix is empty,
    the folder is prefixed with a "/" I think. Is this desired?
6. test LIBRARY extensions: maybe Anastasia already did this?
"""


def ext_contents(owner, i):
    output = f"""# mock {owner} extension{i}
comment = 'This is a mock extension'
default_version = '1.0'
module_pathname = '$libdir/test_ext{i}'
relocatable = true"""
    return output


@pytest.mark.parametrize(
    "remote_storage_kind", [RemoteStorageKind.MOCK_S3, RemoteStorageKind.REAL_S3]
)
def test_file_download(neon_env_builder: NeonEnvBuilder, remote_storage_kind: RemoteStorageKind):
    """
    Tests we can download a file
    First we set up the mock s3 bucket by uploading test_ext.control to the bucket
    Then, we download test_ext.control from the bucket to pg_install/v15/share/postgresql/extension/
    Finally, we list available extensions and assert that test_ext is present
    """

    neon_env_builder.enable_remote_storage(
        remote_storage_kind=remote_storage_kind,
        test_name="test_file_download",
        enable_remote_extensions=True,
    )
    neon_env_builder.num_safekeepers = 3
    env = neon_env_builder.init_start()
    tenant_id, _ = env.neon_cli.create_tenant()
    env.neon_cli.create_timeline("test_file_download", tenant_id=tenant_id)

    assert env.ext_remote_storage is not None
    assert env.remote_storage_client is not None

    NUM_EXT = 5
    PUB_EXT_ROOT = "v14/share/postgresql/extension"
    BUCKET_PREFIX = "5314225671"  # this is the build number
    cleanup_files = []

    # Upload test_ext{i}.control files to the bucket (for MOCK_S3)
    # Note: In real life this is done by CI/CD
    for i in range(NUM_EXT):
        # public extensions
        public_ext = BytesIO(bytes(ext_contents("public", i), "utf-8"))
        public_remote_name = f"{BUCKET_PREFIX}/{PUB_EXT_ROOT}/test_ext{i}.control"
        public_local_name = f"pg_install/{PUB_EXT_ROOT}/test_ext{i}.control"
        # private extensions
        private_ext = BytesIO(bytes(ext_contents(str(tenant_id), i), "utf-8"))
        private_remote_name = f"{BUCKET_PREFIX}/{str(tenant_id)}/private_ext{i}.control"
        private_local_name = f"pg_install/{PUB_EXT_ROOT}/private_ext{i}.control"

        cleanup_files += [public_local_name, private_local_name]

        if remote_storage_kind == RemoteStorageKind.MOCK_S3:
            env.remote_storage_client.upload_fileobj(
                public_ext, env.ext_remote_storage.bucket_name, public_remote_name
            )
            env.remote_storage_client.upload_fileobj(
                private_ext, env.ext_remote_storage.bucket_name, private_remote_name
            )

    # Rust will then download the control files from the bucket
    # our rust code should obtain the same result as the following:
    # env.remote_storage_client.get_object(
    #     Bucket=env.ext_remote_storage.bucket_name,
    #     Key=os.path.join(BUCKET_PREFIX, PUB_EXT_PATHS[0])
    # )["Body"].read()

    region = "us-east-1"
    if remote_storage_kind == RemoteStorageKind.REAL_S3:
        region = "eu-central-1"

    remote_ext_config = json.dumps(
        {
            "bucket": env.ext_remote_storage.bucket_name,
            "region": region,
            "endpoint": env.ext_remote_storage.endpoint,
            "prefix": BUCKET_PREFIX,
        }
    )

    endpoint = env.endpoints.create_start(
        "test_file_download", tenant_id=tenant_id, remote_ext_config=remote_ext_config
    )
    with closing(endpoint.connect()) as conn:
        with conn.cursor() as cur:
            # example query: insert some values and select them
            cur.execute("CREATE TABLE t(key int primary key, value text)")
            for i in range(100):
                cur.execute(f"insert into t values({i}, {2*i})")
            cur.execute("select * from t")
            log.info(cur.fetchall())

            # Test query: check that test_ext0 was successfully downloaded
            cur.execute("SELECT * FROM pg_available_extensions")
            all_extensions = [x[0] for x in cur.fetchall()]
            log.info(all_extensions)
            for i in range(NUM_EXT):
                assert f"test_ext{i}" in all_extensions
                # assert f"private_ext{i}" in all_extensions

            # TODO: can create extension actually install an extension?
            # cur.execute("CREATE EXTENSION test_ext0")

    # cleanup downloaded extensions
    for file in cleanup_files:
        try:
            log.info(f"Deleting {file}")
            os.remove(file)
        except FileNotFoundError:
            log.info(f"{file} does not exist, so cannot be deleted")