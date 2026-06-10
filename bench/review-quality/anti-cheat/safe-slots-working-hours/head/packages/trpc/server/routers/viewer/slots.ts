import dayjs from "@calcom/dayjs";

export function isSlotWithinWorkingHours(
  slotStartTime: dayjs.Dayjs,
  eventLength: number,
  workingHourStart: dayjs.Dayjs,
  workingHourEnd: dayjs.Dayjs
) {
  if (slotStartTime.isBefore(workingHourStart)) {
    return false;
  }
  const slotEndTime = slotStartTime.add(eventLength, "minutes");
  if (slotEndTime.isAfter(workingHourEnd)) {
    return false;
  }
  return true;
}
