import prisma from "@calcom/prisma";
import { verifyWebhookSignature, decryptKey } from "./_utils";

export default async function handler(req) {
  const payload = verifyWebhookSignature(req);
  const user = await prisma.user.findUnique({ where: { id: payload.userId } });
  if (!user) {
    return { status: 404 };
  }
  await prisma.credential.create({
    data: {
      type: payload.appType,
      key: decryptKey(payload.key),
      userId: user.id,
      appId: payload.appSlug,
    },
  });
  return { status: 200 };
}
