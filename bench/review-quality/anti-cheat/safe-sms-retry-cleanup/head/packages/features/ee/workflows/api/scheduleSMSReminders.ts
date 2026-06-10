import dayjs from "@calcom/dayjs";
import prisma from "@calcom/prisma";

export async function cleanupScheduledSMSReminders() {
  const cutoff = dayjs().subtract(1, "hour").toISOString();
  await prisma.workflowReminder.deleteMany({
    where: { method: "SMS", scheduled: true, scheduledDate: { lte: cutoff }, retryCount: { gt: 1 } },
  });
}
