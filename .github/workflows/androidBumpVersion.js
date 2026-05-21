const childProcess = require('child_process');
const fs = require('fs');

// Get commit information
const getCommitHash = () =>
    new Promise((resolve) => {
        childProcess.exec('git rev-parse HEAD', (err, stdout) => {
            if (!err && stdout) {
                return resolve(stdout.replace('\n', ''));
            }
            return resolve('unknown');
        });
    });

const main = async () => {
    const revision = (await getCommitHash()) || 'unknown';
    const version = [new Date().getFullYear(), new Date().getMonth() + 1, new Date().getDate()]; // [...(gitRevisionPlugin.version().split('-')[0].replace('v', '').split('.').map(nb => parseInt(nb || 0, 10)).filter(n => !Number.isNaN(n))), 0, 0, 0]
    const versionName = `${version[0]}.${version[1]}.${version[2]}-${revision.substring(0, 7)}`;

    // The max value of versionCode on PlayStore is 2 100 000 000
    // We will use the timestamp of May, 21th, 2026 and a 30 seconds time range
    // We will not be able to release two differents app build in the same time range
    const baseTimestamp = 1779314400;
    const timeRange = 30;

    // Based on this calc, we will have issues about version code in... 2356
    // I won't be alive for a while, and I hope Google has revised its version number system.
    let versionCode = Math.floor((Math.floor(new Date().getTime() / 1000) - baseTimestamp) / timeRange);

    // Edge case, invalid version code
    if (versionCode <= 0) {
        console.warn('WARNING: versionCode=0, fallback to versionCode=1');
        versionCode = 1;
    }

    // Edge case, not supported by the Playstore
    if (versionCode > 2100000000) {
        console.warn('WARNING: versionCode>2100000000, fallback to versionCode=2100000000');
        versionCode = 2100000000;
    }

    console.log(`auto_version_code=${versionCode}`);
    console.log(`auto_version_name=${versionName}`);

    // Patch versionCode and versonName
    const gradleSettings = fs.readFileSync('./android/app/build.gradle.kts', 'utf-8');
    const patchedGradle = gradleSettings
        .replace('versionCode = 1', `versionCode = ${versionCode}`)
        .replace('versionName = "1.0"', `versionName = "${versionName}"`);

    fs.writeFileSync('./android/app/build.gradle.kts', patchedGradle);
};

main()
    .then(() => process.exit(0))
    .catch((err) => {
        console.error(err);
        process.exit(1);
    });
