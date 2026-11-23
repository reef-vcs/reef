// Pairing (R3.40, ADR 0095): the code `reef remote pair` printed, typed here
// once, traded for this device's token. The daemon answers it only at
// `[remote] origin`.
import { timestampMs } from "@bufbuild/protobuf/wkt";
import { browserConnection, client, type Connection } from "./api/connection";
import { REMOTE_TOKEN_INVALID, type ReefError } from "./api/errors";
import { keepDevice } from "./device";
import { RemoteService } from "./gen/reef/api/v1/remote_pb";
import { storeSession } from "./session";

export const CODE_LENGTH = 8;
export const PAIRING_CODE_INVALID = "PAIRING_CODE_INVALID";

/** A typed or pasted code as it is sent: upper case, letters and digits alone. */
export function typedCode(text: string): string {
  return text.toUpperCase().replace(/[^A-Z0-9]/g, "");
}

/** The connection a code makes, which calls `RedeemPairing` and nothing else. */
export function pairingConnection(code: string): Connection {
  return browserConnection(`reef_pair_${code}`);
}

/** Pairs this device: its token, which is also the stored session. */
export async function pair(
  code: string,
  connect: (code: string) => Connection = pairingConnection,
): Promise<string> {
  const { token, secret } = await client(connect(code), RemoteService).redeemPairing({});
  storeSession(secret);
  keepDevice({
    id: token?.id ?? "",
    name: token?.name ?? "",
    expiresAt: token?.expiresAt ? timestampMs(token.expiresAt) : null,
  });
  return secret;
}

/** A refused code in a person's words. */
export function pairingWords(error: ReefError): string {
  switch (error.reason) {
    case PAIRING_CODE_INVALID:
      return "That code is not valid: it is mistyped, used or expired, or it was made for another address. Run reef remote pair again for a new one.";
    case REMOTE_TOKEN_INVALID:
      return "Too many wrong codes came from here. Wait a minute, then try again.";
    case "":
      return "The daemon did not answer. After several wrong codes it stops answering this device for a minute.";
    default:
      return error.message;
  }
}
