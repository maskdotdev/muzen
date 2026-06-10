import prisma from "@calcom/prisma";
import { verifyWebhookSignature, decryptKey } from "./_utils";

export default async function handler(req) {
  const payload = verifyWebhookSignature(req);
  const user = await prisma.user.findUnique({ where: { id: payload.userId } });
  if (!user) {
    return { status: 404 };
  }
  return { status: 200 };
}
