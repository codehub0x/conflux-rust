# sudo rm -rf /etc/hosts
mv ips ips_old
touch ips
if [[ -f instances ]]
then
	instance=`cat instances`
	response=`aws ec2 describe-instances --instance-ids $instance`
	echo $response | jq ".Reservations[].Instances[].PrivateIpAddress" | tr -d '"' > ips_tmp
	uniq ips_tmp > ips
	rm ips_tmp
fi
echo GET `wc -l ips` IPs
ips=(`cat ips`)
for i in `seq 0 $((${#ips[@]}-1))`
do
#   echo ${ips[$i]} n$i |sudo tee -a /etc/hosts
  ssh -o "StrictHostKeyChecking no" ${ips[$i]} &
done
wait
#scp ips_current lpl@blk:~/ips
#scp -i MyKeyPair.pem ips_current ubuntu@aws:~/ssd/ips
#ssh vm "./tmp.sh"
#rm ipof*
