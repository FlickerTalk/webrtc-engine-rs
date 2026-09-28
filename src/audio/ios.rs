//! iOS backend on the `VoiceProcessingIO` audio unit.

use super::AudioError;

/// A Core Audio result code: zero is success.
pub(crate) type OSStatus = i32;

/// Turns the result of the Core Audio function `call` into a `Result`.
pub(crate) fn check(status: OSStatus, call: &'static str) -> Result<(), AudioError> {
    if status == 0 {
        return Ok(());
    }
    // Many Core Audio errors are four ASCII characters packed into the number, like '!pri'.
    let code = status.to_be_bytes();
    let reason = if code.iter().all(|byte| byte.is_ascii_graphic()) {
        let text: String = code.iter().map(|&byte| char::from(byte)).collect();
        format!("{call} failed: OSStatus {status} ('{text}')")
    } else {
        format!("{call} failed: OSStatus {status}")
    };
    Err(AudioError::Backend(reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_status_is_success() {
        assert_eq!(check(0, "AudioUnitInitialize"), Ok(()));
    }

    #[test]
    fn a_failing_status_names_the_call_and_the_code() {
        let error = check(-10868, "AudioUnitSetProperty").unwrap_err();
        assert_eq!(
            error,
            AudioError::Backend("AudioUnitSetProperty failed: OSStatus -10868".to_owned())
        );
    }

    #[test]
    fn a_four_char_status_is_also_shown_as_text() {
        // '!pri': the session refused to activate or to record (AVAudioSession.ErrorCode).
        let error = check(0x2170_7269, "AudioOutputUnitStart").unwrap_err();
        assert_eq!(
            error,
            AudioError::Backend(
                "AudioOutputUnitStart failed: OSStatus 561017449 ('!pri')".to_owned()
            )
        );
    }
}
