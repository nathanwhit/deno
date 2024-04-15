import type { LoadedConfigSelectors } from "https://esm.sh/v135/@smithy/node-config-provider@2.2.1/dist-types/index.d.ts";
/**
 * @internal
 *
 * @deprecated will be replaced by backend.
 *
 * TODO(s3-express): non-beta value, backend == S3Express.
 */
export declare const S3_EXPRESS_BUCKET_TYPE = "Directory";
/**
 * @internal
 */
export declare const S3_EXPRESS_BACKEND = "S3Express";
/**
 * @internal
 */
export declare const S3_EXPRESS_AUTH_SCHEME = "sigv4-s3express";
/**
 * @internal
 */
export declare const SESSION_TOKEN_QUERY_PARAM = "X-Amz-S3session-Token";
/**
 * @internal
 */
export declare const SESSION_TOKEN_HEADER: string;
/**
 * @internal
 */
export declare const NODE_DISABLE_S3_EXPRESS_SESSION_AUTH_ENV_NAME = "AWS_S3_DISABLE_EXPRESS_SESSION_AUTH";
/**
 * @internal
 */
export declare const NODE_DISABLE_S3_EXPRESS_SESSION_AUTH_INI_NAME = "s3_disable_express_session_auth";
/**
 * @internal
 */
export declare const NODE_DISABLE_S3_EXPRESS_SESSION_AUTH_OPTIONS: LoadedConfigSelectors<boolean>;
