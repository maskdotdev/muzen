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
  return true;
}
