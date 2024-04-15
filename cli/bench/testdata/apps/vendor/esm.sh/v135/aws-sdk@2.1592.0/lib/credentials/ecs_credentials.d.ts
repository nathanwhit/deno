import {RemoteCredentials} from './remote_credentials.d.ts';
export class ECSCredentials extends RemoteCredentials {
        /**
         * Represents credentials received.
         * @param {object} options - Override the default (1s) timeout period.
         */
        constructor(options?: ECSCredentialsOptions);
    }
    interface ECSCredentialsOptions {
        httpOptions?: {
            /**
             * Timeout in milliseconds.
             */
            timeout?: number
        }
        maxRetries?: number
    }
